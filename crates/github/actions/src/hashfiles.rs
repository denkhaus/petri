//! `hashFiles(...)` at spawn time.
//!
//! The frontend lowers a literal-pattern `hashFiles` call in step config to a
//! sentinel (like a secret's); this module replaces it with the hash before the
//! step's process spawns. The hash is computed **in the job environment** — a
//! short `node` program walks `GITHUB_WORKSPACE` — so it sees the same files the
//! step would, wherever the job runs. `node` is already this runner's requirement
//! for JavaScript actions.
//!
//! The algorithm is GitHub's: each matched file's SHA-256 digest, folded through
//! one more SHA-256, as lowercase hex; the empty string when nothing matches.
//! Patterns are relative to `GITHUB_WORKSPACE` and support `*`, `?`, `**` and
//! leading-`!` negation; symbolic links are not followed.

use std::collections::BTreeMap;

use executor::{ExecEnv, ProcessSpec};
use frontend_gha::exprs::{has_hashfiles_sentinel, hashfiles_calls, replace_hashfiles_sentinels};
use ir::Value;
use smol_str::SmolStr;
use steps::{ProcessConfig, StepFailure, ValueOrSecretRef};

/// The step could not compute a `hashFiles` value.
pub const HASHFILES_CLASS: &str = "hashfiles";

/// The line the helper prints its result on.
const MARKER: &str = "petri-hashfiles=";

/// The helper program. Kept dependency-free: `fs`, `path`, `crypto` only.
const HELPER_JS: &str = r#"
const fs=require('fs'),path=require('path'),crypto=require('crypto');
const root=process.env.PETRI_HASHFILES_ROOT;
const patterns=JSON.parse(process.env.PETRI_HASHFILES);
function segMatch(seg,pat){
  let i=0,j=0,star=-1,mark=0;
  while(i<seg.length){
    if(j<pat.length&&(pat[j]==='?'||pat[j]===seg[i])){i++;j++;}
    else if(j<pat.length&&pat[j]==='*'){star=j;j++;mark=i;}
    else if(star>=0){j=star+1;mark++;i=mark;}
    else return false;
  }
  while(j<pat.length&&pat[j]==='*')j++;
  return j===pat.length;
}
function matches(parts,pats){
  const memo=new Map();
  function go(pi,si){
    const k=pi+','+si;
    if(memo.has(k))return memo.get(k);
    let r;
    if(pi===pats.length)r=si===parts.length;
    else if(pats[pi]==='**')r=go(pi+1,si)||(si<parts.length&&go(pi,si+1));
    else r=si<parts.length&&segMatch(parts[si],pats[pi])&&go(pi+1,si+1);
    memo.set(k,r);
    return r;
  }
  return go(0,0);
}
const pos=[],neg=[];
for(const p of patterns){
  const bang=p.startsWith('!');
  (bang?neg:pos).push((bang?p.slice(1):p).split('/').filter(s=>s.length));
}
const files=[];
(function walk(dir,rel){
  let entries=[];
  try{entries=fs.readdirSync(dir,{withFileTypes:true});}catch(e){return;}
  for(const e of entries){
    const r=rel?rel+'/'+e.name:e.name;
    if(e.isSymbolicLink())continue;
    if(e.isDirectory())walk(path.join(dir,e.name),r);
    else if(e.isFile())files.push(r);
  }
})(root,'');
files.sort();
const outer=crypto.createHash('sha256');
let any=false;
for(const f of files){
  const parts=f.split('/');
  if(!pos.some(p=>matches(parts,p)))continue;
  if(neg.some(p=>matches(parts,p)))continue;
  const h=crypto.createHash('sha256');
  h.update(fs.readFileSync(path.join(root,f)));
  outer.update(h.digest());
  any=true;
}
console.log('petri-hashfiles='+(any?outer.digest('hex'):''));
"#;

/// Replace every hashFiles sentinel in the config's `run` and env values with the
/// hash of the matched workspace files, computed in the job environment. A config
/// with no sentinel passes through untouched.
pub(crate) async fn resolve_hashfiles(
    mut process: ProcessConfig,
    env: &dyn ExecEnv,
    github_workspace: &str,
) -> Result<ProcessConfig, StepFailure> {
    let mut calls: BTreeMap<Vec<String>, String> = BTreeMap::new();
    let mut collect = |text: &str| {
        for patterns in hashfiles_calls(text) {
            calls.entry(patterns).or_default();
        }
    };
    collect(&process.run);
    for value in process.env.values() {
        if let ValueOrSecretRef::Literal(Value::String(text)) = value {
            collect(text);
        }
    }
    if calls.is_empty() {
        return Ok(process);
    }

    for (patterns, hash) in &mut calls {
        *hash = compute(env, github_workspace, patterns).await?;
    }

    let splice = |text: &str| -> String {
        replace_hashfiles_sentinels(text, |patterns| -> Result<String, std::convert::Infallible> {
            Ok(calls.get(patterns).cloned().unwrap_or_default())
        })
        .expect("the resolver is infallible")
    };
    if has_hashfiles_sentinel(&process.run) {
        process.run = splice(&process.run);
    }
    for value in process.env.values_mut() {
        if let ValueOrSecretRef::Literal(Value::String(text)) = value
            && has_hashfiles_sentinel(text)
        {
            *text = splice(text);
        }
    }
    Ok(process)
}

/// One `hashFiles(patterns…)`, in the job environment.
async fn compute(
    env: &dyn ExecEnv,
    github_workspace: &str,
    patterns: &[String],
) -> Result<String, StepFailure> {
    let display = patterns.join(", ");
    let fail = |message: String| StepFailure {
        class: HASHFILES_CLASS,
        message,
    };
    let mut helper_env = BTreeMap::new();
    helper_env.insert(
        SmolStr::new("PETRI_HASHFILES"),
        SmolStr::new(serde_json::to_string(patterns).expect("patterns encode")),
    );
    helper_env.insert(
        SmolStr::new("PETRI_HASHFILES_ROOT"),
        SmolStr::new(github_workspace),
    );
    let spec = ProcessSpec {
        program: SmolStr::new("node"),
        args: vec![SmolStr::new("-e"), SmolStr::new(HELPER_JS)],
        env: helper_env,
        cwd: None,
    };
    let mut handle = env.spawn(spec).await.map_err(|e| {
        fail(format!(
            "computing `hashFiles({display})` needs `node` in the job environment: {e}"
        ))
    })?;
    let mut result: Option<String> = None;
    let mut tail: Vec<String> = Vec::new();
    if let Some(mut lines) = handle.lines() {
        while let Some(line) = lines.recv().await {
            if let Some(hash) = line.line.strip_prefix(MARKER) {
                result = Some(hash.to_string());
            } else {
                if tail.len() >= 5 {
                    tail.remove(0);
                }
                tail.push(line.line);
            }
        }
    }
    let status = handle
        .wait()
        .await
        .map_err(|e| fail(format!("the `hashFiles({display})` helper died: {e}")))?;
    if !status.success() {
        return Err(fail(format!(
            "the `hashFiles({display})` helper failed: {}",
            tail.join(" / ")
        )));
    }
    result.ok_or_else(|| fail(format!("the `hashFiles({display})` helper printed no result")))
}
