use super::{command_output, run_shell, Provider};
use crate::{config::Config, fuzzy, types::Item};
use anyhow::Result;

pub struct CalcProvider { history: Vec<(String, String)> }

impl CalcProvider { pub fn new(_config: Config) -> Self { Self { history: Vec::new() } } }

impl Provider for CalcProvider {
    fn name(&self) -> &'static str { "calc" }
    fn pretty_name(&self) -> &'static str { "Calculator" }

    fn query(&mut self, query: &str, limit: usize, exact: bool) -> Vec<Item> {
        let mut out = Vec::new();
        if let Some(result) = calculate(query) {
            let mut item = Item::new(self.name(), query, format!("{query} = {result}"));
            item.subtext = "Copy or save result".into();
            item.icon = "accessories-calculator".into();
            item.actions = vec!["copy".into(), "save".into()];
            item.score = 1_000_000;
            out.push(item);
        }
        for (input, result) in &self.history {
            let text = format!("{input} = {result}");
            if let Some((score, info)) = fuzzy::score(query, &text, exact, "text") {
                let mut item = Item::new(self.name(), input, text);
                item.actions = vec!["copy".into(), "delete".into()];
                item.icon = "accessories-calculator".into();
                item.score = score;
                item.fuzzyinfo = Some(info);
                out.push(item);
            }
        }
        out.truncate(limit);
        out
    }

    fn activate(&mut self, identifier: &str, action: &str, query: &str, _arguments: &str) -> Result<()> {
        let input = if identifier.is_empty() { query } else { identifier };
        let Some(result) = calculate(input) else { return Ok(()); };
        match action {
            "copy" => run_shell(&format!("printf %s '{}' | wl-copy", shell_escape(&result))),
            "save" => { self.history.insert(0, (input.to_string(), result)); Ok(()) }
            "delete" => { self.history.retain(|(i, _)| i != input); Ok(()) }
            _ => anyhow::bail!("unsupported calc action: {action}"),
        }
    }
}

fn calculate(query: &str) -> Option<String> {
    let q = query.trim();
    if q.len() < 2 || !q.chars().any(|c| c.is_ascii_digit()) { return None; }
    if let Some(out) = command_output("qalc", &["-t", q]) { if !out.is_empty() { return Some(out); } }
    meval::eval_str(q).ok().map(|v| trim_float(v))
}

fn trim_float(v: f64) -> String {
    if (v.fract()).abs() < f64::EPSILON { format!("{}", v as i64) } else { format!("{v}") }
}

fn shell_escape(s: &str) -> String { s.replace('\'', "'\\''") }

#[cfg(test)]
mod tests {
    use super::calculate;
    #[test]
    fn evaluates_basic_math() { assert_eq!(calculate("1+2*3").as_deref(), Some("7")); }
}
