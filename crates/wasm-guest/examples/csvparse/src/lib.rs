//! Example plugin with configuration: parse a CSV string payload into rows. Build with
//! `cargo build --release --target wasm32-unknown-unknown` (see `scripts/wasm-examples.sh`).

use edgelink_wasm_guest::{export_node, manifest, Ctx, EveValue, Fill, Level, Msg, Node, Shape};

manifest!("../plugin.toml");

pub struct CsvParse {
    delimiter: char,
    header: bool,
}

/// Split one line into fields. Double-quoted fields may contain the delimiter; `""` inside them
/// is a literal quote.
fn fields(line: &str, delimiter: char) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            ('"', true) => quoted = false,
            ('"', false) if field.is_empty() => quoted = true,
            (c, false) if c == delimiter => out.push(std::mem::take(&mut field)),
            (c, _) => field.push(c),
        }
    }
    if quoted {
        return Err("unterminated quoted field".to_string());
    }
    out.push(field);
    Ok(out)
}

impl CsvParse {
    fn rows(&self, text: &str) -> Result<Vec<EveValue>, String> {
        let mut lines = text.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.is_empty());
        let header = match (self.header, lines.next()) {
            (true, Some(first)) => Some(fields(first, self.delimiter)?),
            (true, None) => return Ok(Vec::new()),
            (false, first) => {
                let mut rows = Vec::new();
                for line in first.into_iter().chain(lines) {
                    let row = fields(line, self.delimiter)?;
                    rows.push(EveValue::Array(row.into_iter().map(EveValue::String).collect()));
                }
                return Ok(rows);
            }
        };
        let header = header.unwrap_or_default();
        let mut rows = Vec::new();
        for (n, line) in lines.enumerate() {
            let row = fields(line, self.delimiter)?;
            if row.len() != header.len() {
                return Err(format!("row {} has {} fields, header has {}", n + 2, row.len(), header.len()));
            }
            let object = header.iter().cloned().zip(row.into_iter().map(EveValue::String)).collect();
            rows.push(EveValue::Object(object));
        }
        Ok(rows)
    }
}

impl Node for CsvParse {
    fn init(config: &Msg) -> Result<Self, String> {
        let delimiter = config.get_str("delimiter").unwrap_or(",");
        let mut chars = delimiter.chars();
        let (Some(delimiter), None) = (chars.next(), chars.next()) else {
            return Err("delimiter must be one character".to_string());
        };
        if delimiter == '"' {
            return Err("the delimiter cannot be a double quote".to_string());
        }
        Ok(Self { delimiter, header: config.get_bool("header").unwrap_or(true) })
    }

    fn on_input(&mut self, ctx: &mut Ctx, mut msg: Msg) -> Result<(), String> {
        let rows = match msg.payload() {
            Some(EveValue::String(text)) => self.rows(text)?,
            _ => return Err("payload must be a CSV string".to_string()),
        };
        ctx.log(Level::Debug, &format!("parsed {} rows", rows.len()));
        ctx.status(Fill::Green, Shape::Dot, &format!("{} rows", rows.len()));
        msg.set_payload(EveValue::Array(rows));
        ctx.emit(0, &msg)
    }
}

export_node!(CsvParse);

#[cfg(test)]
mod tests {
    use super::*;

    fn config(delimiter: &str, header: bool) -> Msg {
        let mut config = Msg::new();
        config.set("delimiter", EveValue::String(delimiter.into()));
        config.set("header", EveValue::Bool(header));
        config
    }

    fn run(node: &mut CsvParse, text: &str) -> Result<EveValue, String> {
        let mut msg = Msg::new();
        msg.set_payload(EveValue::String(text.into()));
        let mut ctx = Ctx::new();
        node.on_input(&mut ctx, msg)?;
        Ok(ctx.record.outputs[0].1.payload().unwrap().clone())
    }

    #[test]
    fn header_rows_become_objects() {
        let mut node = CsvParse::init(&config(";", true)).unwrap();
        let rows = run(&mut node, "name;qty\r\nbolt;4\n\"nut; m3\";\"1\"\"\"\n").unwrap();
        let EveValue::Array(rows) = rows else { panic!("array") };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1],
            EveValue::Object(vec![
                ("name".into(), EveValue::String("nut; m3".into())),
                ("qty".into(), EveValue::String("1\"".into())),
            ])
        );
        assert!(run(&mut node, "a;b\n1").unwrap_err().contains("row 2 has 1 fields"));
    }

    #[test]
    fn without_header_rows_are_arrays_and_bad_config_is_rejected() {
        let mut node = CsvParse::init(&config(",", false)).unwrap();
        let rows = run(&mut node, "a,b\n1,2").unwrap();
        assert_eq!(
            rows,
            EveValue::Array(vec![
                EveValue::Array(vec![EveValue::String("a".into()), EveValue::String("b".into())]),
                EveValue::Array(vec![EveValue::String("1".into()), EveValue::String("2".into())]),
            ])
        );
        assert!(CsvParse::init(&config("ab", true)).is_err());
        assert!(run(&mut node, "\"open").unwrap_err().contains("unterminated"));
    }
}
