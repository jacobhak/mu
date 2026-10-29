//! `yield`: how a subagent with an output schema submits its result. Mirrors omp's tool of the
//! same name: `{data}` for success, `{error}` for failure. A `data` that does not match the
//! schema is returned to the model as a tool error so it can retry.

use super::{ToolOutput, Tools};
use serde::Deserialize;
use serde_json::{Value, json};

pub const DESCRIPTION: &str = "Submit this task's result: `{ data: <your output> }` for success, `{ error: \"message\" }` for failure. Never both; never a bare payload outside `data`. This task declares an output schema: `data` MUST match it, or the call is rejected with the reasons and you must call `yield` again. Prose in your final message is not a result. A successful call ends your run at once: finish all other work first.";

pub fn schema(output: &Value) -> Value {
    json!({"type":"object","properties":{
        "data": output,
        "error":{"type":"string","description":"Failure reason; mutually exclusive with data"}}})
}

/// `12.0` is a valid JSON Schema integer but serde rejects it for integer fields; store `12`.
fn whole_floats_to_ints(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|f| n.is_f64() && f.fract() == 0.0 && f.abs() < 9e15) {
                *v = Value::from(f as i64);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(whole_floats_to_ints),
        Value::Object(o) => o.values_mut().for_each(whole_floats_to_ints),
        _ => {}
    }
}

#[derive(Deserialize)]
pub struct Args {
    data: Option<Value>,
    error: Option<String>,
}

impl Tools {
    pub fn yield_result(&self, a: Args) -> ToolOutput {
        let Some(output) = self.output_schema.borrow().clone() else {
            return ToolOutput::err("yield: this task has no output schema; finish with a normal message");
        };
        let error = a.error.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        let data = a.data.filter(|d| !d.is_null());
        match (data, error) {
            (Some(_), Some(_)) => ToolOutput::err("yield: submit `data` or `error`, never both"),
            (None, None) => {
                ToolOutput::err("yield: submit `{data: ...}` for success or `{error: \"...\"}` for failure")
            }
            (None, Some(e)) => {
                let mut out = ToolOutput::ok("Failure recorded.");
                out.details = Some(json!({"status": "aborted", "error": e}));
                out
            }
            (Some(mut d), None) => match crate::schema::validate(&output, &d) {
                Ok(()) => {
                    whole_floats_to_ints(&mut d);
                    let mut out = ToolOutput::ok("Result submitted.");
                    out.details = Some(json!({"status": "success", "data": d}));
                    out
                }
                Err(errs) => ToolOutput::err(format!(
                    "yield: data does not match the output schema; nothing was submitted. Fix and call yield again.\n{}",
                    errs.join("\n")
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Tools {
        let t = Tools::for_test();
        *t.output_schema.borrow_mut() =
            Some(json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}));
        t
    }

    fn call(t: &Tools, v: Value) -> ToolOutput {
        t.yield_result(serde_json::from_value(v).unwrap())
    }

    #[test]
    fn valid_data_is_recorded_in_details() {
        let out = call(&tools(), json!({"data":{"n":1}}));
        assert!(!out.is_error);
        assert_eq!(out.details, Some(json!({"status":"success","data":{"n":1}})));
    }

    #[test]
    fn invalid_data_is_a_retryable_error_with_paths() {
        let out = call(&tools(), json!({"data":{"n":"x"}}));
        assert!(out.is_error && out.details.is_none());
        assert!(out.text.contains("$.n"), "{}", out.text);
    }

    #[test]
    fn whole_floats_become_integers() {
        let t = tools();
        let out = call(&t, json!({"data":{"n":12.0,"x":[1.0,1.5]}}));
        assert_eq!(out.details, Some(json!({"status":"success","data":{"n":12,"x":[1,1.5]}})));
    }

    #[test]
    fn error_and_misuse() {
        let t = tools();
        let out = call(&t, json!({"error":" nope "}));
        assert_eq!(out.details, Some(json!({"status":"aborted","error":"nope"})));
        assert!(call(&t, json!({})).is_error);
        assert!(call(&t, json!({"data":{"n":1},"error":"x"})).is_error);
        assert!(call(&Tools::for_test(), json!({"data":1})).is_error);
    }

    #[test]
    fn agent_file_output_drives_yield() {
        let root = std::env::temp_dir().join(format!("mu-yield-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        let agent = root.join("agent");
        std::fs::create_dir_all(proj.join(".omp/agents")).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            proj.join(".omp/agents/typed.md"),
            "---\nname: typed\ndescription: typed result\noutput:\n  properties:\n    summary:\n      metadata:\n        description: One line\n      type: string\n    count:\n      type: uint8\n  optionalProperties:\n    files:\n      elements:\n        type: string\n---\nDo it.\n",
        )
        .unwrap();
        let defs = crate::context::discover_agents(&proj, &home, &agent);
        assert_eq!(defs.len(), 1);
        assert!(defs[0].output.is_some());
        let t = Tools::for_test();
        *t.output_schema.borrow_mut() = defs[0].output.clone();
        let data = json!({"summary":"ok","count":3,"files":["a.rs"]});
        let out = call(&t, json!({"data": data}));
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.details, Some(json!({"status":"success","data":data})));
        for bad in
            [json!({"summary":"ok"}), json!({"summary":"ok","count":300}), json!({"summary":"ok","count":1,"extra":1})]
        {
            let out = call(&t, json!({"data": bad}));
            assert!(out.is_error && out.details.is_none());
            assert!(out.text.contains("call yield again"), "{}", out.text);
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
