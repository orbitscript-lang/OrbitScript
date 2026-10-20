//! orbit-lsp driven the way an editor drives it: LSP messages over its
//! standard input and output, against a small workspace on disk.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use orbit_check::{Config, DiskSources, Severity, Workspace};
use serde_json::{Value, json};

const GEO: &str = "\
/// A point in the plane.
pub struct Vec2 { x: float, y: float }

impl Vec2 {
    /// Makes a point.
    pub fn new(x: float, y: float) -> Vec2 { Vec2 { x, y } }

    pub fn len(self) -> float { math::sqrt(self.x * self.x + self.y * self.y) }
}
";

// Line 5 puts text that is longer in UTF-8 than in UTF-16 (and a character
// outside the BMP) before the edit `edits_after_wide_text` makes.
const MAIN: &str = "\
use geo::{Vec2};

let p = Vec2::new(3.0, 4.0);
let n = p.len();
print(n, undefined_thing);
let e = \"ü😀\"; print(e, p.x);
";

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    seq: i64,
    /// Notifications that arrived while waiting for a response.
    notes: Vec<Value>,
}

impl Server {
    fn start(name: &str) -> (Server, PathBuf) {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::write(dir.join("geo.orb"), GEO).unwrap();
        std::fs::write(dir.join("main.orb"), MAIN).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_orbit-lsp"))
            .arg("--stdio")
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let server = Server {
            child,
            input,
            output,
            seq: 0,
            notes: Vec::new(),
        };
        (server, dir)
    }

    fn send(&mut self, message: Value) {
        let body = message.to_string();
        write!(self.input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.input.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "server closed its output"
            );
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(n) = line.strip_prefix("Content-Length:") {
                length = Some(n.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; length.expect("no Content-Length")];
        self.output.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// Send a request and wait for its response, keeping notifications.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.seq += 1;
        let id = self.seq;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let m = self.read();
            if m["id"] == json!(id) {
                return m;
            }
            self.notes.push(m);
        }
    }

    /// A request's result, failing the test on an error response.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let r = self.request(method, params);
        assert!(r.get("error").is_none(), "{method}: {r}");
        r["result"].clone()
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The next diagnostics published for `uri`.
    fn diagnostics(&mut self, uri: &str) -> Vec<Value> {
        loop {
            let m = match self.notes.iter().position(|n| n["params"]["uri"] == uri) {
                Some(i) => self.notes.remove(i),
                None => self.read(),
            };
            if m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri {
                return m["params"]["diagnostics"].as_array().unwrap().clone();
            }
        }
    }

    fn initialize(&mut self, root: &Path, encodings: &[&str]) -> Value {
        let r = self.call(
            "initialize",
            json!({
                "processId": null,
                "rootUri": uri(root, ""),
                "capabilities": {"general": {"positionEncodings": encodings}},
            }),
        );
        self.notify("initialized", json!({}));
        r
    }

    fn open(&mut self, uri: &str, language: &str, text: &str) {
        let doc = json!({"uri": uri, "languageId": language, "version": 1, "text": text});
        self.notify("textDocument/didOpen", json!({"textDocument": doc}));
    }

    /// Shut down and exit, returning the exit code.
    fn stop(mut self) -> i32 {
        assert_eq!(self.call("shutdown", Value::Null), Value::Null);
        self.notify("exit", Value::Null);
        self.child.wait().unwrap().code().unwrap()
    }
}

fn uri(dir: &Path, file: &str) -> String {
    let path = if file.is_empty() {
        dir.to_path_buf()
    } else {
        dir.join(file)
    };
    format!("file://{}", path.display())
}

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}})
}

fn range(r: &Value) -> [u64; 4] {
    let n = |v: &Value| v.as_u64().unwrap();
    [
        n(&r["start"]["line"]),
        n(&r["start"]["character"]),
        n(&r["end"]["line"]),
        n(&r["end"]["character"]),
    ]
}

#[test]
fn session() {
    let (mut s, dir) = Server::start("lsp-session");
    let init = s.initialize(&dir, &["utf-16"]);
    let caps = &init["capabilities"];
    assert_eq!(caps["positionEncoding"], "utf-16");
    assert_eq!(caps["textDocumentSync"]["change"], 2);
    let legend = caps["semanticTokensProvider"]["legend"]["tokenTypes"]
        .as_array()
        .unwrap()
        .len();

    let main = uri(&dir, "main.orb");
    let geo = uri(&dir, "geo.orb");
    s.open(&main, "orbitscript", MAIN);
    let diags = s.diagnostics(&main);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("undefined_thing")
    );
    assert_eq!(diags[0]["severity"], 1);
    assert_eq!(range(&diags[0]["range"]), [4, 9, 4, 24]);

    // `Vec2` in `Vec2::new`: its declaration and doc comment, and where it is.
    let hover = s.call("textDocument/hover", at(&main, 2, 9));
    let text = hover["contents"]["value"].as_str().unwrap();
    assert!(text.contains("pub struct Vec2"), "{text}");
    assert!(text.contains("A point in the plane."), "{text}");
    let def = s.call("textDocument/definition", at(&main, 2, 9));
    assert_eq!(def["uri"], geo);
    assert_eq!(range(&def["range"]), [1, 11, 1, 15]);

    // The cursor right after a name still finds it.
    let mut refs = at(&main, 2, 5);
    refs["context"] = json!({"includeDeclaration": true});
    let refs = s.call("textDocument/references", refs);
    let lines: Vec<u64> = refs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| range(&r["range"])[0])
        .collect();
    assert_eq!(lines, [2, 3, 5]);
    let highlights = s.call("textDocument/documentHighlight", at(&main, 3, 8));
    assert_eq!(highlights.as_array().unwrap().len(), 3);

    let items = s.call("textDocument/completion", at(&main, 3, 10));
    let labels: Vec<&str> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"len") && labels.contains(&"x"),
        "{labels:?}"
    );

    let sig = s.call("textDocument/signatureHelp", at(&main, 2, 18));
    let label = sig["signatures"][0]["label"].as_str().unwrap();
    assert!(label.contains("x: float"), "{label}");
    assert_eq!(
        sig["signatures"][0]["parameters"].as_array().unwrap().len(),
        2
    );
    assert_eq!(sig["activeParameter"], 0);
    let sig = s.call("textDocument/signatureHelp", at(&main, 2, 23));
    assert_eq!(sig["activeParameter"], 1);

    let hints = s.call(
        "textDocument/inlayHint",
        json!({"textDocument": {"uri": main}, "range": {
            "start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}}}),
    );
    let first = &hints.as_array().unwrap()[0];
    assert_eq!(first["label"], ": Vec2");
    assert_eq!(first["position"], json!({"line": 2, "character": 5}));

    let rename = s.call("textDocument/prepareRename", at(&main, 2, 4));
    assert_eq!(rename["placeholder"], "p");
    let mut params = at(&main, 2, 4);
    params["newName"] = json!("q");
    let edit = s.call("textDocument/rename", params);
    assert_eq!(edit["changes"][&main].as_array().unwrap().len(), 3);
    let mut params = at(&main, 2, 4);
    params["newName"] = json!("not a name");
    let bad = s.request("textDocument/rename", params);
    assert!(bad["error"]["message"].is_string(), "{bad}");

    let tokens = s.call(
        "textDocument/semanticTokens/full",
        json!({"textDocument": {"uri": main}}),
    );
    let data = tokens["data"].as_array().unwrap();
    assert!(!data.is_empty() && data.len() % 5 == 0);
    assert!(
        data.chunks(5)
            .all(|t| (t[3].as_u64().unwrap() as usize) < legend)
    );
    // `use` is a keyword at 0:0.
    assert_eq!(&data[..4], &[json!(0), json!(0), json!(3), json!(10)]);

    // `3.0`, then the call's arguments, the parentheses, the file.
    let mut params = json!({"textDocument": {"uri": main}, "positions": [
        {"line": 2, "character": 19}]});
    let sel = &s.call("textDocument/selectionRange", params.take())[0];
    assert_eq!(range(&sel["range"]), [2, 18, 2, 21]);
    assert_eq!(range(&sel["parent"]["range"]), [2, 18, 2, 26]);
    assert_eq!(range(&sel["parent"]["parent"]["range"]), [2, 17, 2, 27]);
    let top = &sel["parent"]["parent"]["parent"];
    assert_eq!(range(&top["range"])[..2], [0, 0]);
    assert!(top.get("parent").is_none(), "{top}");

    // A file that isn't open is read from disk.
    let outline = s.call(
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": geo}}),
    );
    let names: Vec<&str> = outline
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Vec2"), "{outline}");
    let folds = s.call(
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": geo}}),
    );
    assert!(
        folds
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["startLine"] == 3 && f["endLine"] == 7),
        "{folds}"
    );
    let found = s.call("workspace/symbol", json!({"query": "vec"}));
    let found = found.as_array().unwrap();
    assert!(
        found
            .iter()
            .any(|f| f["name"] == "Vec2" && f["location"]["uri"] == geo),
        "{found:?}"
    );

    // An incremental edit after wide characters: `p.x` on line 5 becomes
    // `n`, counted in UTF-16 (`ü` is one unit, `😀` two).
    s.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": main, "version": 2}, "contentChanges": [
            {"range": {"start": {"line": 5, "character": 24}, "end": {"line": 5, "character": 27}},
             "text": "n"},
            {"range": {"start": {"line": 4, "character": 9}, "end": {"line": 4, "character": 24}},
             "text": "p.x"},
        ]}),
    );
    let diags = s.diagnostics(&main);
    assert!(diags.is_empty(), "{diags:?}");
    // Now `p` is read on lines 2, 3 and 4 but not 5.
    let refs = s.call("textDocument/references", at(&main, 2, 4));
    let lines: Vec<u64> = refs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| range(&r["range"])[0])
        .collect();
    assert_eq!(lines, [2, 3, 4]);

    assert_eq!(s.stop(), 0);
}

#[test]
fn utf8_positions_when_offered() {
    let (mut s, dir) = Server::start("lsp-utf8");
    let init = s.initialize(&dir, &["utf-8", "utf-16"]);
    assert_eq!(init["capabilities"]["positionEncoding"], "utf-8");
    let main = uri(&dir, "main.orb");
    s.open(&main, "orbitscript", MAIN);
    s.diagnostics(&main);
    // `p` in `p.x` on line 5: `let e = "ü😀"; print(e, ` is 27 bytes.
    let def = s.call("textDocument/definition", at(&main, 5, 27));
    assert_eq!(range(&def["range"]), [2, 4, 2, 5]);
    assert_eq!(s.stop(), 0);
}

#[test]
fn lua_files_get_syntax_errors() {
    let (mut s, dir) = Server::start("lsp-lua");
    s.initialize(&dir, &[]);
    let script = uri(&dir, "broken.lua");
    s.open(&script, "lua", "local x = 1\nif x then\n");
    let diags = s.diagnostics(&script);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["severity"], 1);
    s.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": script, "version": 2},
               "contentChanges": [{"text": "local x = 1\nif x then end\n"}]}),
    );
    assert!(s.diagnostics(&script).is_empty());
    let outline = s.call(
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": script}}),
    );
    assert_eq!(outline, json!([]));
    assert_eq!(s.stop(), 0);
}

#[test]
fn protocol_errors() {
    let (mut s, dir) = Server::start("lsp-errors");
    let early = s.request("textDocument/hover", at("file:///x.orb", 0, 0));
    assert_eq!(early["error"]["code"], -32002);
    s.initialize(&dir, &[]);
    let unknown = s.request("orbit/nothing", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    let body = "{not json";
    write!(s.input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    s.input.flush().unwrap();
    let parse = s.read();
    assert_eq!(parse["error"]["code"], -32700);
    assert_eq!(parse["id"], Value::Null);
    // A notification nobody handles is ignored, and the server carries on.
    s.notify("$/setTrace", json!({"value": "off"}));
    let r = s.call("textDocument/hover", at(&uri(&dir, "main.orb"), 2, 9));
    assert!(r["contents"]["value"].is_string());
    // `exit` without `shutdown` first ends with 1, as LSP says.
    s.notify("exit", Value::Null);
    assert_eq!(s.child.wait().unwrap().code(), Some(1));
}

/// The bundled scripts pass the checker, against the host module's stub.
#[test]
fn scripts_check_clean() {
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
    let config = Config {
        root: scripts.clone(),
        ..Config::default()
    };
    let mut ws = Workspace::new(config, Box::new(DiskSources));
    for entry in std::fs::read_dir(scripts.join("lsp")).unwrap() {
        ws.add_file(&entry.unwrap().path());
    }
    let mut problems = Vec::new();
    let results: Vec<_> = ws
        .check_all()
        .into_iter()
        .map(|(f, d)| (f, d.to_vec()))
        .collect();
    for (file, diags) in results {
        for d in diags {
            if d.severity != Severity::Hint {
                let (line, col) = ws.line_col(file, d.span.start);
                let path = ws.path(file).display();
                problems.push(format!("{path}:{line}:{col}: {}", d.message));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
