//! Built-in / external reference filtering — port of `isBuiltInOrExternal`
//! (Go subset). A `pkg.X` where `pkg` is a Go stdlib package, or a bare Go
//! built-in, is not a project symbol and must not produce an edge.

use super::graph::Graph;
use crate::types::{Language, UnresolvedReference};

/// Go standard-library package names (port of `GO_STDLIB_PACKAGES`).
const GO_STDLIB_PACKAGES: &[&str] = &[
    "fmt", "os", "io", "net", "http", "log", "math", "sort", "sync", "time", "path", "bytes",
    "strings", "strconv", "errors", "context", "json", "xml", "csv", "html", "template", "regexp",
    "reflect", "runtime", "testing", "flag", "bufio", "crypto", "encoding", "filepath", "hash",
    "mime", "rand", "signal", "sql", "syscall", "unicode", "unsafe", "atomic", "binary", "debug",
    "exec", "heap", "ring", "scanner", "tar", "zip", "gzip", "zlib", "tls", "url", "user", "pprof",
    "trace", "ast", "build", "parser", "printer", "token", "types", "cgo", "plugin", "race",
    "ioutil", "utilruntime", "utilwait", "utilnet",
];

/// Go built-in functions/types (port of `GO_BUILT_INS`).
const GO_BUILT_INS: &[&str] = &[
    "make", "new", "len", "cap", "append", "copy", "delete", "close", "panic", "recover", "print",
    "println", "complex", "real", "imag", "error", "nil", "true", "false", "iota", "int", "int8",
    "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr", "float32",
    "float64", "complex64", "complex128", "string", "bool", "byte", "rune", "any",
];

pub fn is_builtin_or_external(_g: &Graph, r: &UnresolvedReference) -> bool {
    if r.language != Some(Language::Go) {
        return false;
    }
    let name = &r.reference_name;
    if let Some(dot) = name.find('.') {
        let pkg = &name[..dot];
        if GO_STDLIB_PACKAGES.contains(&pkg) {
            return true;
        }
    }
    GO_BUILT_INS.contains(&name.as_str())
}
