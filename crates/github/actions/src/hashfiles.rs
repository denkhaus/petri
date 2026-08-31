//! `hashFiles(...)` at spawn time.
//!
//! The frontend lowers a literal-pattern `hashFiles` call in step config to a
//! sentinel (like a secret's); this module replaces it with the hash before the
//! step's process spawns. The hash is computed **in the job environment** — a
//! short `node` program walks `GITHUB_WORKSPACE` — so it sees the same files
//! the step would, wherever the job runs. `node` is already this runner's
//! requirement for JavaScript actions.
//!
//! The algorithm is GitHub's: each matched file's SHA-256 digest, folded
//! through one more SHA-256, as lowercase hex; the empty string when nothing
//! matches. Patterns are relative to `GITHUB_WORKSPACE` and support `*`, `?`,
//! `**` and leading-`!` negation; symbolic links are not followed.

use std::collections::BTreeMap;
use std::convert::Infallible;

use executor::{ExecEnv, ProcessSpec};
use frontend_gha::exprs::{has_hashfiles_sentinel, hashfiles_calls, replace_hashfiles_sentinels};
use ir::FailureClass;
use smol_str::SmolStr;
use steps::StepFailure;

use crate::session::ResolvedProcess;

/// The step could not compute a `hashFiles` value.
const HASHFILES_CLASS: FailureClass = FailureClass::new_static("hashfiles");

/// The line the helper prints its result on.
const MARKER: &str = "petri-hashfiles=";

/// The helper program. Kept dependency-free: `fs`, `path`, `crypto` only.
const HELPER_JS: &str = r"
const fs=require('fs'),path=require('path'),crypto=require('crypto');
const root=process.env.PETRI_HASHFILES_ROOT;
const calls=JSON.parse(process.env.PETRI_HASHFILES);
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
function compile(patterns){
  const pos=[],neg=[];
  for(const p of patterns){
    const bang=p.startsWith('!');
    (bang?neg:pos).push((bang?p.slice(1):p).split('/').filter(s=>s.length));
  }
  return {pos,neg,outer:crypto.createHash('sha256'),any:false};
}
const specs=calls.map(compile);
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
for(const f of files){
  const parts=f.split('/');
  const selected=specs.filter(s=>
    s.pos.some(p=>matches(parts,p))&&!s.neg.some(p=>matches(parts,p)));
  if(selected.length===0)continue;
  const h=crypto.createHash('sha256');
  h.update(fs.readFileSync(path.join(root,f)));
  const digest=h.digest();
  for(const spec of selected){spec.outer.update(digest);spec.any=true;}
}
console.log('petri-hashfiles='+JSON.stringify(
  specs.map(s=>s.any?s.outer.digest('hex'):'')));
";

/// Replace every hashFiles sentinel in the config's `run` and env values with
/// the hash of the matched workspace files, computed in the job environment. A
/// config with no sentinel passes through untouched.
pub(crate) async fn resolve_hashfiles(
    process: ResolvedProcess,
    env: &dyn ExecEnv,
    github_workspace: &str,
) -> Result<ResolvedProcess, StepFailure> {
    let calls = resolved_calls(process.texts(), env, github_workspace).await?;
    if calls.is_empty() {
        return Ok(process);
    }
    let process = process
        .try_map_texts(|text| {
            if has_hashfiles_sentinel(text) {
                Ok::<_, Infallible>(Some(splice(text, &calls)))
            } else {
                Ok(None)
            }
        })
        .expect("the resolver is infallible");
    Ok(process)
}

/// The hash of every distinct `hashFiles(patterns…)` call across `texts`, in
/// one workspace walk. Empty when no text carries a sentinel. Shared by the
/// config path and the gate path, so both hash the same way.
pub(crate) async fn resolved_calls<'a>(
    texts: impl Iterator<Item = &'a str>,
    env: &dyn ExecEnv,
    github_workspace: &str,
) -> Result<BTreeMap<Vec<String>, String>, StepFailure> {
    let mut calls: BTreeMap<Vec<String>, String> = BTreeMap::new();
    for text in texts {
        for patterns in hashfiles_calls(text) {
            calls.entry(patterns).or_default();
        }
    }
    if calls.is_empty() {
        return Ok(calls);
    }
    let patterns: Vec<Vec<String>> = calls.keys().cloned().collect();
    let hashes = compute(env, github_workspace, &patterns).await?;
    for (patterns, hash) in patterns.into_iter().zip(hashes) {
        calls.insert(patterns, hash);
    }
    Ok(calls)
}

/// One text with its sentinels replaced by the resolved hashes; a call the map
/// does not hold reads as the empty string, GitHub's "nothing matched".
pub(crate) fn splice(text: &str, calls: &BTreeMap<Vec<String>, String>) -> String {
    replace_hashfiles_sentinels(text, |patterns| -> Result<String, Infallible> {
        Ok(calls.get(patterns).cloned().unwrap_or_default())
    })
    .expect("the resolver is infallible")
}

/// Every distinct `hashFiles(patterns…)` in one step, in one workspace walk.
#[tracing::instrument(
    name = "github.hashfiles",
    level = "debug",
    skip_all,
    fields(call_count = patterns.len())
)]
async fn compute(
    env: &dyn ExecEnv,
    github_workspace: &str,
    patterns: &[Vec<String>],
) -> Result<Vec<String>, StepFailure> {
    let display = patterns
        .iter()
        .map(|call| call.join(", "))
        .collect::<Vec<_>>()
        .join("; ");
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
        args:    vec![SmolStr::new("-e"), SmolStr::new(HELPER_JS)],
        env:     helper_env,
        cwd:     None,
    };
    let mut handle = env.spawn(spec).await.map_err(|e| {
        fail(format!(
            "computing `hashFiles({display})` needs `node` in the job environment: {e}"
        ))
    })?;
    let mut result: Option<Vec<String>> = None;
    // Kept until the helper is reaped: a crash explains a bad marker line better
    // than the parse error does, so the exit status is reported first.
    let mut parse_error: Option<String> = None;
    let mut tail: Vec<String> = Vec::new();
    if let Some(mut lines) = handle.lines() {
        while let Some(line) = lines.recv().await {
            if let Some(hashes) = line.line.strip_prefix(MARKER) {
                match serde_json::from_str(hashes) {
                    Ok(hashes) => {
                        result = Some(hashes);
                        parse_error = None;
                    }
                    Err(e) => {
                        result = None;
                        parse_error = Some(e.to_string());
                    }
                }
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
    if !status.is_success() {
        return Err(fail(format!(
            "the `hashFiles({display})` helper failed: {}",
            tail.join(" / ")
        )));
    }
    if let Some(e) = parse_error {
        return Err(fail(format!(
            "the `hashFiles({display})` helper printed an unreadable result: {e}"
        )));
    }
    let result = result.ok_or_else(|| {
        fail(format!(
            "the `hashFiles({display})` helper printed no result"
        ))
    })?;
    if result.len() != patterns.len() {
        return Err(fail(format!(
            "the `hashFiles({display})` helper returned {} results for {} calls",
            result.len(),
            patterns.len()
        )));
    }
    Ok(result)
}
