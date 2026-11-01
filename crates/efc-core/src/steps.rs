//! Шаги пакета (SPEC 5.2.1): имена шагов, подстановка `${шаг.поле}`, проверка `expect`.

use crate::model::{Expect, Op, Request};
use serde_json::Value;
use std::collections::HashMap;

/// Возможность сервера: шаги пакета (`id`, `${…}`, `expect`, `always`, `onError`/`timeout` на шаге).
pub const FEATURE_STEPS: &str = "steps";
/// Возможность сервера: `lock` у запроса.
pub const FEATURE_LOCK: &str = "lock";

/// Результат выполненного шага — источник подстановок.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub exit: i32,
    pub output: String,
    pub data: Value,
}

/// Допустимое имя шага.
pub fn valid_step_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Имена шагов: заданные или `s1`, `s2`, …. Ошибка — недопустимое или повторённое имя.
pub fn step_ids(ops: &[Op]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let id = op.id.clone().unwrap_or_else(|| format!("s{}", i + 1));
        if !valid_step_id(&id) {
            return Err(format!("недопустимое имя шага «{id}» (латиница, цифры, _ и -, до 32 символов)"));
        }
        if out.contains(&id) {
            return Err(format!("имя шага «{id}» повторяется"));
        }
        out.push(id);
    }
    Ok(out)
}

/// Использует ли запрос шаги (то, что сервер без `steps` молча проигнорировал бы).
pub fn uses_steps(req: &Request) -> bool {
    req.batch.as_ref().map(|b| b.iter().any(op_uses_steps)).unwrap_or(false)
}

fn op_uses_steps(op: &Op) -> bool {
    op.id.is_some()
        || op.expect.is_some()
        || op.always
        || op.on_error.is_some()
        || op.timeout.is_some()
        || op.args.iter().any(|a| a.contains("${"))
        || value_has_ref(&op.params)
}

fn value_has_ref(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("${"),
        Value::Array(a) => a.iter().any(value_has_ref),
        Value::Object(m) => m.values().any(value_has_ref),
        _ => false,
    }
}

/// Значение по ссылке `шаг.путь`: `exitCode`, `output`, `ok` или путь в `data` через точку
/// (индексы массивов — числами: `items.0.name`).
fn lookup(reference: &str, results: &HashMap<String, StepResult>) -> Result<Value, String> {
    let (step, path) = reference.split_once('.').ok_or_else(|| format!("${{{reference}}}: нужно «шаг.поле»"))?;
    let r = results.get(step).ok_or_else(|| format!("${{{reference}}}: шаг «{step}» ещё не выполнен или не существует"))?;
    match path {
        "exitCode" => return Ok(Value::from(r.exit)),
        "output" => return Ok(Value::String(r.output.clone())),
        "ok" => return Ok(Value::Bool(r.exit == 0)),
        _ => {}
    }
    let mut cur = &r.data;
    for part in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(part),
            Value::Array(a) => part.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        }
        .ok_or_else(|| format!("${{{reference}}}: у шага «{step}» нет поля «{path}»"))?;
    }
    Ok(cur.clone())
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Подставить ссылки в строку. Строка ровно из одной ссылки — значение как есть (число, объект…).
pub fn subst_value(s: &str, results: &HashMap<String, StepResult>) -> Result<Value, String> {
    let t = s.trim();
    if t.starts_with("${") && t.ends_with('}') && t.matches("${").count() == 1 && t == s {
        return lookup(&t[2..t.len() - 1], results);
    }
    Ok(Value::String(subst_text(s, results)?))
}

/// Подставить ссылки в строку, результат — строка.
pub fn subst_text(s: &str, results: &HashMap<String, StepResult>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let j = tail.find('}').ok_or_else(|| format!("незакрытая ссылка в «{s}»"))?;
        out.push_str(&as_text(&lookup(&tail[..j], results)?));
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Подставить ссылки во всё дерево JSON.
pub fn subst_json(v: &Value, results: &HashMap<String, StepResult>) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => subst_value(s, results)?,
        Value::Array(a) => Value::Array(a.iter().map(|x| subst_json(x, results)).collect::<Result<_, _>>()?),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| Ok((k.clone(), subst_json(x, results)?))).collect::<Result<_, String>>()?),
        other => other.clone(),
    })
}

/// Шаг с подставленными ссылками (аргументы, params, expect).
pub fn resolve(op: &Op, results: &HashMap<String, StepResult>) -> Result<Op, String> {
    let mut o = op.clone();
    o.args = op.args.iter().map(|a| subst_text(a, results)).collect::<Result<_, _>>()?;
    o.params = subst_json(&op.params, results)?;
    if let Some(e) = &op.expect {
        o.expect = Some(Expect {
            exit: e.exit,
            output: e.output.as_deref().map(|s| subst_text(s, results)).transpose()?,
            not_output: e.not_output.as_deref().map(|s| subst_text(s, results)).transpose()?,
            data: match &e.data {
                Some(m) => Some(m.iter().map(|(k, v)| Ok((k.clone(), subst_json(v, results)?))).collect::<Result<_, String>>()?),
                None => None,
            },
        });
    }
    Ok(o)
}

/// Проверить результат шага. Пусто — всё сходится. Без `expect` — ожидается код 0.
pub fn check_expect(e: Option<&Expect>, exit: i32, output: &str, data: &Value) -> Vec<String> {
    let mut bad = Vec::new();
    let want_exit = e.and_then(|e| e.exit).unwrap_or(0);
    if exit != want_exit {
        bad.push(format!("код {exit}, ожидался {want_exit}"));
    }
    let Some(e) = e else { return bad };
    if let Some(s) = &e.output {
        if !output.contains(s.as_str()) {
            bad.push(format!("в выводе нет «{s}»"));
        }
    }
    if let Some(s) = &e.not_output {
        if output.contains(s.as_str()) {
            bad.push(format!("в выводе есть «{s}»"));
        }
    }
    if let Some(m) = &e.data {
        for (path, want) in m {
            let mut cur = Some(data);
            for part in path.split('.') {
                cur = cur.and_then(|c| match c {
                    Value::Object(o) => o.get(part),
                    Value::Array(a) => part.parse::<usize>().ok().and_then(|i| a.get(i)),
                    _ => None,
                });
            }
            match cur {
                Some(got) if got == want => {}
                // 3 и 3.0, "3" и 3 — одно и то же для проверки
                Some(got) if as_text(got) == as_text(want) => {}
                Some(got) => bad.push(format!("data.{path} = {got}, ожидалось {want}")),
                None => bad.push(format!("в data нет {path}")),
            }
        }
    }
    bad
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn results() -> HashMap<String, StepResult> {
        let mut r = HashMap::new();
        r.insert("play".into(), StepResult { exit: 0, output: "ok\n".into(), data: json!({ "logSince": 1532, "scene": { "name": "Main" }, "list": [ "a", "b" ] }) });
        r
    }

    #[test]
    fn substitution() {
        let r = results();
        assert_eq!(subst_text("--since ${play.logSince}", &r).unwrap(), "--since 1532");
        assert_eq!(subst_value("${play.logSince}", &r).unwrap(), json!(1532));
        assert_eq!(subst_value("${play.scene}", &r).unwrap(), json!({ "name": "Main" }));
        assert_eq!(subst_text("${play.scene.name}/${play.list.1}", &r).unwrap(), "Main/b");
        assert_eq!(subst_text("${play.exitCode} ${play.ok}", &r).unwrap(), "0 true");
        assert!(subst_text("${nope.x}", &r).unwrap_err().contains("не выполнен"));
        assert!(subst_text("${play.missing}", &r).unwrap_err().contains("нет поля"));
        assert!(subst_text("${play.x", &r).is_err());
        assert_eq!(subst_json(&json!({ "a": ["${play.logSince}", "x${play.logSince}"] }), &r).unwrap(), json!({ "a": [1532, "x1532"] }));
    }

    #[test]
    fn expectations() {
        let d = json!({ "rows": 0, "n": 3 });
        assert!(check_expect(None, 0, "", &d).is_empty());
        assert_eq!(check_expect(None, 1, "", &d).len(), 1);
        let e = Expect { exit: Some(3), output: Some("нет".into()), not_output: Some("pong".into()), data: Some(json!({ "rows": 0, "n": "3" }).as_object().unwrap().clone()) };
        assert!(check_expect(Some(&e), 3, "нет команды", &d).is_empty(), "{:?}", check_expect(Some(&e), 3, "нет команды", &d));
        assert_eq!(check_expect(Some(&e), 0, "pong", &d).len(), 3);
    }

    #[test]
    fn ids() {
        let ops = vec![Op { command: "a".into(), ..Default::default() }, Op { id: Some("play".into()), command: "b".into(), ..Default::default() }];
        assert_eq!(step_ids(&ops).unwrap(), vec!["s1", "play"]);
        let dup = vec![Op { id: Some("x".into()), ..Default::default() }, Op { id: Some("x".into()), ..Default::default() }];
        assert!(step_ids(&dup).is_err());
        assert!(!valid_step_id("a b"));
    }
}
