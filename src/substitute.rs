//! Flux postBuild variable substitution, and redaction of the values it
//! pulls from Secrets.
//!
//! The evaluator mirrors fluxcd/pkg/envsubst as kustomize-controller runs
//! it here, in strict mode (`StrictPostBuildSubstitutions=true`): `$$` is a
//! literal `$`, `${VAR}` must be set, and `${VAR:-default}` (or `=`, `:=`,
//! all the same in Flux) falls back when the value is empty or unset. Bare
//! `$VAR` is not expanded, and `${VAR-default}` is an error, as in Flux
//! despite its README. Checked against `flux envsubst --strict` over every
//! file in the homelab repo plus edge cases.
//!
//! Flux's other operators are refused, not evaluated: `${#VAR}`,
//! `${VAR:1:2}`, `${VAR^^}`, `${VAR/a/b}`, `${VAR#x}` and so on each derive
//! a new string from a value. Redaction works by recognising a Secret's value
//! in the output, so a caller could use them to read a Secret back piece by
//! piece, one character or its length at a time.

use std::collections::HashMap;

use serde_json::Value;

/// Expands `text` with `vars`. Errors name the variable and operator, never
/// a value.
pub fn eval(text: &str, vars: &HashMap<String, String>) -> Result<String, String> {
    Parser {
        chars: text.chars().collect(),
        pos: 0,
        vars,
    }
    .text(false)
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    vars: &'a HashMap<String, String>,
}

impl Parser<'_> {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    /// Text up to the end of input, or, inside a default, up to its `}`.
    fn text(&mut self, in_default: bool) -> Result<String, String> {
        let mut out = String::new();
        loop {
            match (self.peek(0), self.peek(1)) {
                (None, _) if in_default => return Err("missing closing brace".into()),
                (None, _) => return Ok(out),
                (Some('}'), _) if in_default => {
                    self.pos += 1;
                    return Ok(out);
                }
                (Some('$'), Some('$')) => {
                    out.push('$');
                    self.pos += 2;
                }
                (Some('$'), Some('{')) => {
                    self.pos += 2;
                    out.push_str(&self.expression()?);
                }
                (Some(c), _) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// The body of `${...}`, after the `${`.
    fn expression(&mut self) -> Result<String, String> {
        if self.peek(0) == Some('#') {
            return Err("${#VAR} is not supported: only ${VAR} and ${VAR:-default}".into());
        }
        let name = self.identifier();
        if name.is_empty() {
            return Err("bad substitution: ${ must be followed by a variable name".into());
        }
        match (self.peek(0), self.peek(1)) {
            (Some('}'), _) => {
                self.pos += 1;
                self.vars
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| format!("variable not set (strict mode): {name}"))
            }
            (Some(':'), Some('-' | '=')) => {
                self.pos += 2;
                self.default(&name)
            }
            (Some('='), _) => {
                self.pos += 1;
                self.default(&name)
            }
            (Some(op @ (':' | ',' | '^' | '/' | '#' | '%')), _) => Err(format!(
                "${{{name}{op}...}} is not supported: only ${{VAR}} and ${{VAR:-default}}"
            )),
            _ => Err(format!("${{{name}: missing closing brace")),
        }
    }

    fn default(&mut self, name: &str) -> Result<String, String> {
        let fallback = self.text(true)?;
        match self.vars.get(name) {
            Some(value) if !value.is_empty() => Ok(value.clone()),
            _ => Ok(fallback),
        }
    }

    fn identifier(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek(0) {
            let ok =
                c == '_' || c.is_ascii_alphabetic() || (self.pos > start && c.is_ascii_digit());
            if !ok {
                break;
            }
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }
}

/// Replaces Secret-sourced values with `<redacted:NAME>`.
#[derive(Default)]
pub struct Redactor {
    /// (needle, replacement), longest needle first so a value containing
    /// another is replaced whole.
    needles: Vec<(String, String)>,
}

impl Redactor {
    pub fn new<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut needles = Vec::new();
        for (name, value) in secrets {
            if value.is_empty() {
                continue;
            }
            let replacement = format!("<redacted:{name}>");
            // API errors quote values JSON-style, escaping quotes and
            // backslashes; catch that spelling too.
            let escaped = serde_json::to_string(value).unwrap_or_default();
            let escaped = escaped.trim_matches('"');
            if escaped != value {
                needles.push((escaped.to_string(), replacement.clone()));
            }
            needles.push((value.to_string(), replacement));
        }
        needles.sort_by_key(|(needle, _)| std::cmp::Reverse(needle.len()));
        Self { needles }
    }

    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (needle, replacement) in &self.needles {
            out = out.replace(needle, replacement);
        }
        out
    }

    /// Redacts every string and map key in `value`. Done on objects before
    /// they're rendered, so it doesn't depend on how YAML quotes a value.
    pub fn value(&self, value: &mut Value) {
        if self.needles.is_empty() {
            return;
        }
        match value {
            Value::String(s) => *s = self.text(s),
            Value::Array(items) => items.iter_mut().for_each(|v| self.value(v)),
            Value::Object(map) => {
                let entries = std::mem::take(map);
                for (key, mut v) in entries {
                    self.value(&mut v);
                    map.insert(self.text(&key), v);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> HashMap<String, String> {
        HashMap::from([
            ("DOMAIN".into(), "example.com".into()),
            ("EMPTY".into(), String::new()),
        ])
    }

    #[test]
    fn expands_plain_and_default_forms() {
        let v = vars();
        assert_eq!(eval("a.${DOMAIN}", &v).unwrap(), "a.example.com");
        for op in [":-", ":=", "="] {
            let set = format!("${{DOMAIN{op}x}}");
            let empty = format!("${{EMPTY{op}x}}");
            let unset = format!("${{UNSET{op}x}}");
            assert_eq!(eval(&set, &v).unwrap(), "example.com", "{op}");
            assert_eq!(eval(&empty, &v).unwrap(), "x", "{op}");
            assert_eq!(eval(&unset, &v).unwrap(), "x", "{op}");
        }
        assert_eq!(eval("${UNSET:=${DOMAIN}}", &v).unwrap(), "example.com");
        // Flux takes default text literally, backslashes included.
        assert_eq!(eval("${UNSET:-a\\/b}", &v).unwrap(), "a\\/b");
        assert!(eval("${UNSET-x}", &v).is_err());
    }

    #[test]
    fn escapes_and_bare_dollars_match_flux() {
        let v = vars();
        assert_eq!(eval("$${DOMAIN}", &v).unwrap(), "${DOMAIN}");
        assert_eq!(eval("$DOMAIN costs $5", &v).unwrap(), "$DOMAIN costs $5");
        assert_eq!(eval("a\\/b", &v).unwrap(), "a\\/b");
    }

    #[test]
    fn unset_variables_fail_in_strict_mode() {
        let err = eval("${UNSET}", &vars()).unwrap_err();
        assert!(err.contains("UNSET"), "{err}");
    }

    #[test]
    fn refuses_operators_that_derive_strings_from_a_value() {
        let v = vars();
        for expr in [
            "${#DOMAIN}",
            "${DOMAIN:0:1}",
            "${DOMAIN^^}",
            "${DOMAIN,}",
            "${DOMAIN/e/x}",
            "${DOMAIN#*.}",
            "${DOMAIN%.*}",
        ] {
            let err = eval(expr, &v).unwrap_err();
            assert!(err.contains("not supported"), "{expr}: {err}");
            assert!(!err.contains("example"), "{expr} leaked: {err}");
        }
    }

    #[test]
    fn rejects_malformed_expressions() {
        let v = vars();
        assert!(eval("${DOMAIN", &v).is_err());
        assert!(eval("${UNSET:-x", &v).is_err());
        assert!(eval("${{ github.sha }}", &v).is_err());
    }

    #[test]
    fn redacts_text_values_and_keys() {
        let r = Redactor::new([("DOMAIN", "example.com"), ("SUB", "example"), ("NONE", "")]);
        assert_eq!(
            r.text("a.example.com and example"),
            "a.<redacted:DOMAIN> and <redacted:SUB>"
        );

        let mut v = serde_json::json!({
            "metadata": {"labels": {"example.com/x": "1"}},
            "spec": {"hosts": ["a.example.com"]}
        });
        r.value(&mut v);
        let s = v.to_string();
        assert!(!s.contains("example"), "{s}");
        assert!(s.contains("<redacted:DOMAIN>/x"), "{s}");
    }

    #[test]
    fn redacts_json_escaped_spellings() {
        let r = Redactor::new([("Q", "a\"b")]);
        assert_eq!(
            r.text(r#"invalid value "a\"b""#),
            r#"invalid value "<redacted:Q>""#
        );
    }
}
