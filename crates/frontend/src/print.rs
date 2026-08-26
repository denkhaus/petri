//! A stable text rendering of a lowered graph.
//!
//! For `petri check --print-graph`: reviewable by eye, diffable by the corpus
//! harness. Expressions print in the native syntax, which the strict lowering reads
//! back, so the output is close to a native-format document for the graph.

use std::fmt::Write;

use ir::{Expr, ExprId, ExprTable, Graph, Guard, JoinPolicy, RuntimeTarget};

pub fn print_graph(graph: &Graph) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "graph");
    let _ = writeln!(
        out,
        "  entry: [{}]",
        graph
            .entry
            .iter()
            .filter_map(|id| graph.node(*id))
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !graph.params.is_empty() {
        let _ = writeln!(
            out,
            "  params: {}",
            graph
                .params
                .keys()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for scope in &graph.scopes {
        let runtime = match &scope.runtime.target {
            RuntimeTarget::HostProcess => "host".to_string(),
            RuntimeTarget::Docker { image, .. } => format!("docker {image}"),
        };
        let _ = write!(out, "  scope {}: {runtime}", scope.id);
        if !scope.runtime.requirements.is_empty() {
            let _ = write!(
                out,
                " requires [{}]",
                scope
                    .runtime
                    .requirements
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let _ = writeln!(out);
        for (key, value) in &scope.env {
            let rendered = match value {
                ir::ExprOrValue::Value(v) => v.to_string(),
                ir::ExprOrValue::Expr(id) => {
                    format!("${{{{ {} }}}}", print_expr(&graph.exprs, *id))
                }
            };
            let _ = writeln!(out, "    env {key} = {rendered}");
        }
    }
    for node in &graph.nodes {
        let join = match node.join {
            JoinPolicy::All => "all".to_string(),
            JoinPolicy::Any => "any".to_string(),
            JoinPolicy::Quorum { n } => format!("quorum {n}"),
        };
        let _ = writeln!(
            out,
            "  node {} \"{}\" scope={} step={} join={join}",
            node.id, node.name, node.scope, node.step.kind
        );
        if let Some(pre) = node.precondition {
            let _ = writeln!(out, "    if: {}", print_expr(&graph.exprs, pre));
        }
        if node.budget.max_firings != 1 || node.budget.timeout.as_secs() != 3600 {
            let _ = writeln!(
                out,
                "    budget: max_firings={} timeout={}s",
                node.budget.max_firings,
                node.budget.timeout.as_secs()
            );
        }
        if node.retry.max_attempts.get() != 1 {
            let _ = writeln!(out, "    retry: max_attempts={}", node.retry.max_attempts);
        }
        if let Some(ir::Expansion::ForEach {
            items,
            target,
            max_parallel,
            fail_fast,
        }) = &node.expand
        {
            let target = match target {
                ir::ExpandTarget::Node => "node".to_string(),
                ir::ExpandTarget::Subgraph { entry, exit } => format!(
                    "subgraph {}..{}",
                    graph.node(*entry).map_or("?", |n| n.name.as_str()),
                    graph.node(*exit).map_or("?", |n| n.name.as_str())
                ),
            };
            let _ = writeln!(
                out,
                "    for_each: {} target={target} max_parallel={} fail_fast={fail_fast}",
                print_expr(&graph.exprs, *items),
                max_parallel.map_or("-".to_string(), |n| n.to_string())
            );
        }
        if !node.step.config.is_null() {
            let _ = writeln!(
                out,
                "    config: {}",
                print_config(&graph.exprs, &node.step.config)
            );
        }
        for (gi, group) in node.routing.groups.iter().enumerate() {
            let fallthrough = match group.fallthrough {
                ir::Fallthrough::NoEmit => "",
                ir::Fallthrough::Error => " (must match)",
            };
            let _ = writeln!(out, "    group {gi}{fallthrough}:");
            for arm in &group.arms {
                let to = graph.node(arm.to).map_or("?", |n| n.name.as_str());
                let guard = match arm.guard {
                    Guard::Always => "always".to_string(),
                    Guard::Expr(id) => format!("when {}", print_expr(&graph.exprs, id)),
                };
                let _ = write!(out, "      -> {to} [{}] {guard}", arm.id);
                if let Some(map) = arm.map {
                    let _ = write!(out, " map {}", print_expr(&graph.exprs, map));
                }
                if arm.back {
                    let _ = write!(out, " back");
                }
                let _ = writeln!(out);
            }
        }
    }
    out
}

/// Render a config value, showing `{"$expr": id}` placeholders as the expression.
fn print_config(table: &ExprTable, value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(id) = map
                .get(ir::validate::EXPR_PLACEHOLDER_KEY)
                .and_then(|v| v.as_u64())
            {
                return format!("${{{{ {} }}}}", print_expr(table, ExprId::new(id as u32)));
            }
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", print_config(table, v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        serde_json::Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(|v| print_config(table, v)).collect();
            format!("[{}]", inner.join(", "))
        }
        serde_json::Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}

/// Print a core expression in the native syntax.
pub fn print_expr(table: &ExprTable, id: ExprId) -> String {
    let mut out = String::new();
    write_expr(table, id, &mut out, 0);
    out
}

fn write_expr(table: &ExprTable, id: ExprId, out: &mut String, depth: usize) {
    if depth > 64 {
        out.push('…');
        return;
    }
    let Some(expr) = table.get(id) else {
        let _ = write!(out, "<expr {id}?>");
        return;
    };
    let d = depth + 1;
    match expr {
        Expr::Lit(v) => match v {
            serde_json::Value::String(s) => {
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
            other => out.push_str(&other.to_string()),
        },
        Expr::Var(name) => out.push_str(name),
        Expr::Field(base, name) => {
            write_expr(table, *base, out, d);
            out.push('.');
            out.push_str(name);
        }
        Expr::Index(base, idx) => {
            write_expr(table, *base, out, d);
            out.push('[');
            write_expr(table, *idx, out, d);
            out.push(']');
        }
        Expr::Unary(op, arg) => {
            out.push_str(match op {
                ir::UnOp::Not => "!",
                ir::UnOp::Neg => "-",
            });
            write_expr(table, *arg, out, d);
        }
        Expr::Binary(op, l, r) => {
            out.push('(');
            write_expr(table, *l, out, d);
            let _ = write!(
                out,
                " {} ",
                match op {
                    ir::BinOp::Eq => "==",
                    ir::BinOp::Ne => "!=",
                    ir::BinOp::Lt => "<",
                    ir::BinOp::Le => "<=",
                    ir::BinOp::Gt => ">",
                    ir::BinOp::Ge => ">=",
                    ir::BinOp::And => "&&",
                    ir::BinOp::Or => "||",
                    ir::BinOp::Add => "+",
                    ir::BinOp::Sub => "-",
                    ir::BinOp::Mul => "*",
                    ir::BinOp::Div => "/",
                    ir::BinOp::Rem => "%",
                    ir::BinOp::Concat => "++",
                }
            );
            write_expr(table, *r, out, d);
            out.push(')');
        }
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => {
            out.push_str("if(");
            write_expr(table, *cond, out, d);
            out.push_str(", ");
            write_expr(table, *then, out, d);
            out.push_str(", ");
            write_expr(table, *otherwise, out, d);
            out.push(')');
        }
        Expr::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(table, *item, out, d);
            }
            out.push(']');
        }
        Expr::Object(fields) => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{k}: ");
                write_expr(table, *v, out, d);
            }
            out.push('}');
        }
        Expr::Call(name, args) => {
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(table, *a, out, d);
            }
            out.push(')');
        }
    }
}
