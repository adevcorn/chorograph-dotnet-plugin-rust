use chorograph_plugin_sdk_rust::prelude::*;
use once_cell::sync::Lazy;
use regex::Regex;

// ---------------------------------------------------------------------------
// Compiled regexes (compiled once, safe in WASM)
// ---------------------------------------------------------------------------

static RE_TARGET_FRAMEWORK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<TargetFramework>(.*?)</TargetFramework>").unwrap());

static RE_CLASS_ROUTE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\[Route\s*\(\s*"([^"]+)"\s*\)\]"#).unwrap());

static RE_HTTP_METHOD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\[(Http(?:Get|Post|Put|Delete|Patch|Head|Options))\s*(?:\(\s*"([^"]*)"\s*\))?\]"#)
        .unwrap()
});

static RE_MAP_ROUTE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"app\s*\.\s*Map(Get|Post|Put|Delete|Patch)\s*\(\s*"([^"]+)""#).unwrap()
});

// Matches: .MapGroup("/prefix")  →  captures the prefix string
static RE_MAP_GROUP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\.MapGroup\s*\(\s*"([^"]+)"\s*\)"#).unwrap());

// Matches: MapGroup("prefix") without a leading dot — for Program.cs variable usage:
//   var g = app.MapGroup("/v1/errors"); g.MapGroup("bad-request").MapPostValidate();
// Also used in chain parsing. Same capture as RE_MAP_GROUP but without requiring leading dot.
static RE_MAP_GROUP_ANY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"MapGroup\s*\(\s*"([^"]+)"\s*\)"#).unwrap());

// Matches a public/private extension method on IEndpointRouteBuilder:
//   [modifiers] static IEndpointRouteBuilder MethodName(this IEndpointRouteBuilder app)
static RE_ENDPOINT_METHOD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:public|private|internal)\s+static\s+\S+\s+([A-Z][A-Za-z0-9_]*)\s*\(\s*this\s+IEndpointRouteBuilder\b").unwrap()
});

static RE_RAZOR_HANDLER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:public|protected)\s+[\w<>\[\]]+\s+(On(Get|Post|Put|Delete|Patch)\w*)\s*\(")
        .unwrap()
});

static RE_MAIN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"static\s+(?:async\s+)?(?:void|Task|int)\s+Main\s*\(").unwrap());

static RE_EXECUTE_ASYNC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:protected|public)\s+override\s+async\s+Task\s+ExecuteAsync\s*\(").unwrap()
});

static RE_CREATE_MAUI_APP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:public|internal)\s+static\s+MauiApp\s+CreateMauiApp\s*\(").unwrap()
});

static RE_ON_STARTUP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:protected\s+override\s+void\s+OnStartup|void\s+Application_Startup)\s*\(")
        .unwrap()
});

// Matches the last PascalCase identifier before '(' on a method signature line.
static RE_METHOD_NAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b([A-Z][A-Za-z0-9_]*)\s*\(").unwrap());

// ---------------------------------------------------------------------------
// Plugin entry points
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn init() {
    log!("Dotnet Plugin Loaded");
}

#[chorograph_plugin]
pub fn identify_project(root: String, files: Vec<String>) -> Option<ProjectProfile> {
    // 1. Find project file (.csproj or .fsproj)
    let project_file_name = files
        .iter()
        .find(|f| f.ends_with(".csproj") || f.ends_with(".fsproj"))?;

    // 2. Build absolute path and read the project file
    let full_path = join_path(&root, project_file_name);
    let csproj_content = match read_host_file(&full_path) {
        Ok(c) => c,
        Err(e) => {
            log!("Failed to read project file {}: {:?}", full_path, e);
            return None;
        }
    };

    // 3. Detect language tag
    let mut tags = vec![".NET".to_string()];
    if project_file_name.ends_with(".csproj") {
        tags.push("C#".to_string());
    } else {
        tags.push("F#".to_string());
    }

    // 4. Detect category — reads Program.cs for Web SDK projects
    let category = detect_category(&root, &files, &csproj_content);

    // 5. Detect target framework tag
    if let Some(tf) = detect_target_framework(&csproj_content) {
        tags.push(tf);
    }

    // 6. Detect entry points based on category
    let entry_points = detect_entry_points(&root, &files, &category);

    Some(ProjectProfile {
        category,
        tags,
        entry_points,
    })
}

#[chorograph_plugin]
pub fn handle_action(_action_id: String, _payload: serde_json::Value) {
    // No-op for now
}

#[chorograph_plugin]
pub fn detect_run_status(root: String) -> Option<RunStatus> {
    // Only meaningful for web project categories — detect category first by
    // re-running the lightweight category check without file list (csproj only).
    // We do a quick check: look for a .csproj in the root directory listing and
    // read it to see if it's a Web SDK project.
    let csproj_content = find_and_read_csproj(&root)?;

    let is_web = csproj_content.contains("Sdk=\"Microsoft.NET.Sdk.Web\"")
        || csproj_content.contains("Sdk='Microsoft.NET.Sdk.Web'")
        || csproj_content.contains("<Sdk Name=\"Microsoft.NET.Sdk.Web\"")
        || csproj_content.contains("<Sdk Name='Microsoft.NET.Sdk.Web'");

    if !is_web {
        return None;
    }

    // Read Properties/launchSettings.json to find the applicationUrl.
    let launch_settings_path = join_path(&root, "Properties/launchSettings.json");
    let launch_settings = match read_host_file(&launch_settings_path) {
        Ok(s) => s,
        Err(_) => return None,
    };

    let url = parse_application_url(&launch_settings)?;
    let port = parse_port_from_url(&url)?;

    let is_running = tcp_probe("localhost", port);

    Some(RunStatus {
        is_running,
        url: if is_running { Some(url) } else { None },
        pid: None,
        resources: vec![],
    })
}

/// Try to find and read a .csproj file directly in the project root.
fn find_and_read_csproj(root: &str) -> Option<String> {
    // We don't have a directory listing here, so try common patterns.
    // The host file API supports arbitrary paths, so we try to read a sentinel
    // file that lists directory contents — but we don't have that.
    // Instead, ask the host to read a known path: the csproj must be at
    // <root>/<basename>.csproj. Derive basename from the last path component of root.
    let basename = root.trim_end_matches('/').rsplit('/').next().unwrap_or("");

    if !basename.is_empty() {
        let candidate = join_path(root, &format!("{}.csproj", basename));
        if let Ok(content) = read_host_file(&candidate) {
            return Some(content);
        }
        // Try .fsproj too
        let candidate_fs = join_path(root, &format!("{}.fsproj", basename));
        if let Ok(content) = read_host_file(&candidate_fs) {
            return Some(content);
        }
    }
    None
}

/// Parse the first `applicationUrl` value from a launchSettings.json string.
/// Prefers https:// URLs; falls back to http://.
fn parse_application_url(json: &str) -> Option<String> {
    // Rather than pulling in a full JSON parser we do a targeted string search.
    // The key always appears as: "applicationUrl": "..."
    // Multiple URLs may be separated by ";" — take the first https:// one,
    // falling back to the first http:// one.
    let mut http_fallback: Option<String> = None;

    // Find all occurrences of "applicationUrl": "<value>"
    let mut search = json;
    while let Some(key_pos) = search.find("\"applicationUrl\"") {
        let after_key = &search[key_pos + "\"applicationUrl\"".len()..];
        // Find the colon and then the opening quote
        if let Some(colon_pos) = after_key.find(':') {
            let after_colon = after_key[colon_pos + 1..].trim_start();
            if after_colon.starts_with('"') {
                let inner = &after_colon[1..];
                if let Some(end_quote) = inner.find('"') {
                    let raw_value = &inner[..end_quote];
                    // Value may be "url1;url2;..." — split on ';'
                    for part in raw_value.split(';') {
                        let part = part.trim();
                        if part.starts_with("https://") {
                            return Some(part.to_string());
                        }
                        if part.starts_with("http://") && http_fallback.is_none() {
                            http_fallback = Some(part.to_string());
                        }
                    }
                }
            }
        }
        // Advance past this occurrence
        let skip = key_pos + 1;
        if skip >= search.len() {
            break;
        }
        search = &search[skip..];
    }

    http_fallback
}

/// Parse the port number from a URL string like "http://localhost:5241" or "https://localhost:7001".
fn parse_port_from_url(url: &str) -> Option<u16> {
    // Find the last ':' that is followed by digits (the port part).
    // Strip scheme first: skip past "://"
    let after_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };
    // Now we have "localhost:5241" or "localhost:7001/path"
    if let Some(colon_pos) = after_scheme.rfind(':') {
        let port_str = &after_scheme[colon_pos + 1..];
        // Trim any path suffix
        let port_str = port_str.split('/').next().unwrap_or(port_str);
        port_str.parse::<u16>().ok()
    } else {
        // No explicit port — use default
        if url.starts_with("https://") {
            Some(443)
        } else {
            Some(80)
        }
    }
}

// ---------------------------------------------------------------------------
// Category detection
// ---------------------------------------------------------------------------

fn detect_category(root: &str, files: &[String], csproj: &str) -> String {
    // Log the first 500 chars of the csproj so we can see what SDK is declared.
    let preview: String = csproj.chars().take(500).collect();
    log!("detect_category root={} csproj_preview={}", root, preview);

    // Match Web SDK regardless of quote style (double or single) and regardless
    // of whether it appears as an XML attribute on <Project Sdk="..."> or as a
    // standalone <Sdk Name="..." /> element.
    let is_web_sdk = csproj.contains("Sdk=\"Microsoft.NET.Sdk.Web\"")
        || csproj.contains("Sdk='Microsoft.NET.Sdk.Web'")
        || csproj.contains("<Sdk Name=\"Microsoft.NET.Sdk.Web\"")
        || csproj.contains("<Sdk Name='Microsoft.NET.Sdk.Web'");

    if is_web_sdk {
        // The SDK attribute is in the .csproj, but AddControllers/MapControllers
        // live in Program.cs / Startup.cs — so we read those files.
        let program_content = read_first_matching_file(root, files, |name| {
            let lower = name.to_lowercase();
            lower == "program.cs" || lower == "startup.cs"
        });

        if let Some(ref prog) = program_content {
            log!("detect_category: found program file, checking patterns");
            if prog.contains("AddControllersWithViews") || prog.contains("AddMvc") {
                return "WebApp (MVC/Pages)".to_string();
            }
            // Match both `AddControllers(` and `.AddControllers(` (builder pattern)
            if prog.contains("AddControllers") || prog.contains("MapControllers") {
                return "WebAPI".to_string();
            }
            // Minimal API registrations (app.MapGet/MapPost etc.) → WebAPI
            if RE_MAP_ROUTE.is_match(prog) {
                return "WebAPI".to_string();
            }
            // MapGroup wires up endpoint groups defined elsewhere → WebAPI
            if prog.contains(".MapGroup(") {
                return "WebAPI".to_string();
            }
            // Razor Pages
            if prog.contains("AddRazorPages") || prog.contains("MapRazorPages") {
                return "WebApp (MVC/Pages)".to_string();
            }
        } else {
            log!("detect_category: no program.cs/startup.cs found in files list");
        }

        // Fallback: if there are controller files it's a WebAPI even without an
        // explicit AddControllers() call (e.g. conventions-based registration).
        let has_controllers = files.iter().any(|f| {
            let lower = f.to_lowercase();
            lower.ends_with("controller.cs")
                || lower.contains("/controllers/")
                || lower.contains("\\controllers\\")
        });
        if has_controllers {
            log!(
                "detect_category: no program.cs pattern matched but controllers dir found → WebAPI"
            );
            return "WebAPI".to_string();
        }

        return "WebApp".to_string();
    }

    // Worker SDK — same quote-style variants
    let is_worker_sdk = csproj.contains("Sdk=\"Microsoft.NET.Sdk.Worker\"")
        || csproj.contains("Sdk='Microsoft.NET.Sdk.Worker'")
        || csproj.contains("<Sdk Name=\"Microsoft.NET.Sdk.Worker\"")
        || csproj.contains("<Sdk Name='Microsoft.NET.Sdk.Worker'");

    if is_worker_sdk {
        return "Worker".to_string();
    }

    if csproj.contains("<OutputType>Exe</OutputType>") {
        if csproj.contains("UseMaui")
            || csproj.contains("UseWindowsForms")
            || csproj.contains("UseWPF")
        {
            return "NativeApp".to_string();
        }
        return "ConsoleApp".to_string();
    }

    log!("detect_category: fell through to Library for root={}", root);
    "Library".to_string()
}

// ---------------------------------------------------------------------------
// Target framework detection
// ---------------------------------------------------------------------------

fn detect_target_framework(csproj: &str) -> Option<String> {
    // Fast path for common versions (avoids regex overhead in happy path)
    for ver in &["net9.0", "net8.0", "net7.0", "net6.0"] {
        if csproj.contains(&format!("<TargetFramework>{}</TargetFramework>", ver)) {
            return Some(ver.to_string());
        }
    }
    let caps = RE_TARGET_FRAMEWORK.captures(csproj)?;
    caps.get(1).map(|m| m.as_str().to_string())
}

// ---------------------------------------------------------------------------
// Entry point detection dispatcher
// ---------------------------------------------------------------------------

fn detect_entry_points(root: &str, files: &[String], category: &str) -> Vec<EntryPoint> {
    match category {
        "WebAPI" => detect_webapi_entry_points(root, files),
        "WebApp (MVC/Pages)" | "WebApp" => detect_webapp_entry_points(root, files),
        "ConsoleApp" => detect_console_entry_points(root, files),
        "Worker" => detect_worker_entry_points(root, files),
        "NativeApp" => detect_native_entry_points(root, files),
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// WebAPI: controller route attributes + minimal API
// ---------------------------------------------------------------------------

fn detect_webapi_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    // --- Controller scan ---
    // Try LSP first: ask the host for workspace symbols, then filter for
    // method-level symbols (SymbolKind 6) in controller files.  If the LSP
    // session is not ready (returns Err or empty), fall back to regex.
    let lsp_controller_eps = lsp_controller_entry_points(root, files);
    if !lsp_controller_eps.is_empty() {
        entry_points.extend(lsp_controller_eps);
    } else {
        // Regex fallback: scan controller files directly.
        let controller_files: Vec<&String> = files
            .iter()
            .filter(|f| {
                let lower = f.to_lowercase();
                lower.ends_with("controller.cs")
                    || lower.contains("/controllers/")
                    || lower.contains("\\controllers\\")
            })
            .collect();

        for rel_path in controller_files {
            let full = join_path(root, rel_path);
            if let Ok(src) = read_host_file(&full) {
                let mut eps = scan_controller_routes(rel_path, &src);
                entry_points.append(&mut eps);
            }
        }
    }

    // --- Minimal API scan ---
    // Try LSP first: resolve three-level route groups across endpoint files and
    // Program.cs. Falls back to a flat regex scan if LSP is unavailable or
    // returns nothing useful.
    let lsp_minimal_eps = lsp_minimal_api_entry_points(root, files);
    if !lsp_minimal_eps.is_empty() {
        log!(
            "[lsp_minimal_api] got {} entry points via LSP",
            lsp_minimal_eps.len()
        );
        entry_points.extend(lsp_minimal_eps);
    } else {
        log!("[lsp_minimal_api] falling back to regex scan");
        // Regex fallback: scan all .cs files for app.Map* registrations.
        let cs_files: Vec<&String> = files
            .iter()
            .filter(|f| f.to_lowercase().ends_with(".cs"))
            .collect();

        for rel_path in cs_files {
            let full = join_path(root, rel_path);
            if let Ok(src) = read_host_file(&full) {
                let mut eps = scan_minimal_api_routes(rel_path, &src);
                entry_points.append(&mut eps);
            }
        }
    }

    entry_points
}

/// Ask the host LSP for workspace symbols, filter for method-level symbols
/// (SymbolKind 6) in controller files, then read each file to extract the
/// HTTP verb and route from the surrounding attribute annotations.
/// Returns an empty vec if LSP is unavailable or returns no relevant symbols —
/// callers should fall back to `scan_controller_routes` in that case.
fn lsp_controller_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    // Build a quick lookup set of known relative paths for controller files.
    let controller_paths: std::collections::HashSet<&str> = files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            lower.ends_with("controller.cs")
                || lower.contains("/controllers/")
                || lower.contains("\\controllers\\")
        })
        .map(|s| s.as_str())
        .collect();

    if controller_paths.is_empty() {
        return vec![];
    }

    let symbols = match workspace_symbols_from_host(root) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    if symbols.is_empty() {
        return vec![];
    }

    let mut entry_points: Vec<EntryPoint> = Vec::new();

    // Cache file contents so we don't re-read the same file for every symbol.
    let mut file_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for sym in &symbols {
        // Only method-level symbols (LSP SymbolKind 6).
        if sym.kind != 6 {
            continue;
        }

        // Match LSP's absolute file_path against our relative controller paths.
        // LSP returns absolute paths; our `files` list is relative to root.
        let rel_path = controller_paths.iter().find(|&&rel| {
            let lower_fp = sym.file_path.to_lowercase();
            let lower_rel = rel.to_lowercase().replace('\\', "/");
            lower_fp.ends_with(&lower_rel)
        });

        let rel_path = match rel_path {
            Some(r) => *r,
            None => continue,
        };

        // Load and cache file lines.
        if !file_cache.contains_key(rel_path) {
            let full = join_path(root, rel_path);
            match read_host_file(&full) {
                Ok(src) => {
                    file_cache.insert(
                        rel_path.to_string(),
                        src.lines().map(|l| l.to_string()).collect(),
                    );
                }
                Err(_) => continue,
            }
        }

        let lines = match file_cache.get(rel_path) {
            Some(l) => l,
            None => continue,
        };

        // sym.line is 0-based from LSP; convert to 0-based index.
        let method_line_idx = sym.line as usize;

        // Find the class-level [Route] for base route — scan upward from the
        // method line, stopping at a reasonable limit.
        let base_route = lines[..method_line_idx.min(lines.len())]
            .iter()
            .rev()
            .take(200)
            .find_map(|l| {
                RE_CLASS_ROUTE
                    .captures(l)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string())
            })
            .unwrap_or_default();

        // Scan up to 5 lines *before* the method line for [HttpVerb] attribute.
        let attr_line = lines[..method_line_idx.min(lines.len())]
            .iter()
            .rev()
            .take(5)
            .find_map(|l| RE_HTTP_METHOD.captures(l).map(|c| c));

        let (verb, route) = match attr_line {
            Some(caps) => {
                let verb_full = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let verb = verb_full
                    .strip_prefix("Http")
                    .unwrap_or(verb_full)
                    .to_uppercase();
                let fragment = caps.get(2).map(|m| m.as_str()).unwrap_or("").trim();
                (verb, build_route(&base_route, fragment))
            }
            // No [HttpVerb] attribute found nearby — not an action method.
            None => continue,
        };

        let label = format!("{} {}", verb, route);

        entry_points.push(EntryPoint {
            label,
            path: rel_path.to_string(),
            // LSP line is 0-based; EntryPoint.line is 1-based.
            line: Some(method_line_idx as u32 + 1),
            method: Some(verb),
            description: Some(sym.name.clone()),
            detection_source: Some("lsp".to_string()),
        });
    }

    entry_points
}

/// LSP-assisted minimal API entry point detection.
///
/// Uses workspace symbols (kind 6 = Method) to find endpoint extension methods in
/// `*Endpoints.cs` files, then does targeted source reads to resolve the three-level
/// route structure:
///
///   Program.cs:         app.MapGroup("/v1/outer").MapPublicMethod();
///   XxxEndpoints.cs:    public MapPublicMethod → app.MapGroup("/inner").MapLeaf1()...
///   XxxEndpoints.cs:    private MapLeaf1       → app.MapGet("/leaf", handler)
///
/// Also handles the ProblemApi pattern where Program.cs itself supplies two levels:
///   var g = app.MapGroup("/v1/errors");
///   g.MapGroup("bad-request").MapPostValidate();
///
/// Returns an empty vec if LSP is unavailable or returns no useful symbols.
fn lsp_minimal_api_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    // -----------------------------------------------------------------------
    // Step 1 — get workspace symbols from host
    // -----------------------------------------------------------------------
    let symbols = match workspace_symbols_from_host(root) {
        Ok(s) if !s.is_empty() => s,
        _ => return vec![],
    };

    // -----------------------------------------------------------------------
    // Step 2 — collect method-level (kind=6) symbols in *Endpoints.cs files
    // -----------------------------------------------------------------------
    // Build a set of relative endpoint file paths (lowercased) for fast lookup.
    let endpoint_files: std::collections::HashSet<&str> = files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            lower.ends_with("endpoints.cs") && !lower.ends_with("controller.cs")
        })
        .map(|s| s.as_str())
        .collect();

    if endpoint_files.is_empty() {
        return vec![];
    }

    // Map: relative_path → list of (method_name, lsp_line_0based)
    let mut endpoint_methods_by_file: std::collections::HashMap<String, Vec<(String, u32)>> =
        std::collections::HashMap::new();

    for sym in &symbols {
        if sym.kind != 6 {
            continue;
        }
        let rel = endpoint_files.iter().find(|&&rel| {
            let lower_fp = sym.file_path.to_lowercase().replace('\\', "/");
            let lower_rel = rel.to_lowercase().replace('\\', "/");
            lower_fp.ends_with(&lower_rel)
        });
        if let Some(&rel) = rel {
            endpoint_methods_by_file
                .entry(rel.to_string())
                .or_default()
                .push((sym.name.clone(), sym.line));
        }
    }

    if endpoint_methods_by_file.is_empty() {
        return vec![];
    }

    // -----------------------------------------------------------------------
    // Step 3 — read all Program.cs files and build two maps:
    //   a) outer_prefix[PublicMethodName] = "/v1/outer"
    //      (from: app.MapGroup("/v1/outer").MapPublicMethod())
    //   b) program_group_then_method[PublicMethodName] = "/some-prefix"
    //      (from: someVar.MapGroup("prefix").MapPublicMethod())
    // Both patterns appear in Program.cs.
    // -----------------------------------------------------------------------
    //   outer_prefix: public extension method name → outer group prefix
    let mut outer_prefix: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    let program_files: Vec<&String> = files
        .iter()
        .filter(|f| {
            let bare = f.rsplit('/').next().unwrap_or(f.as_str()).to_lowercase();
            bare == "program.cs" || bare == "startup.cs"
        })
        .collect();

    for rel_prog in &program_files {
        let full = join_path(root, rel_prog);
        let src = match read_host_file(&full) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // We walk the source character by character to find all
        // MapGroup("x") followed (anywhere on the same logical line/chain)
        // by .MethodName() — handling both:
        //   app.MapGroup("/v1/outer").MapDrawsEndpoints()
        //   g.MapGroup("bad-request").MapPostValidate()
        //   var g = app.MapGroup("/v1/errors");  ← stored group, handled separately
        //
        // Strategy: iterate over all `MapGroup` occurrences; for each, scan
        // ahead up to 300 chars (covers chained calls) for `.MethodName(`.
        // Record MethodName → group prefix.
        let src_bytes = src.as_bytes();
        let src_len = src_bytes.len();

        for mg_cap in RE_MAP_GROUP_ANY.captures_iter(&src) {
            let group_prefix = mg_cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let mg_end = mg_cap.get(0).map(|m| m.end()).unwrap_or(0);

            // Scan ahead up to 300 bytes for `.MethodName(`
            let scan_end = (mg_end + 300).min(src_len);
            let snippet = &src[mg_end..scan_end];

            // Find all method calls of the form `.MethodName(`
            for m_cap in RE_ENDPOINT_CALL.captures_iter(snippet) {
                if let Some(name) = m_cap.get(1) {
                    let method_name = name.as_str().to_string();
                    // Only record if not already present (first wins = outermost)
                    outer_prefix
                        .entry(method_name)
                        .or_insert_with(|| group_prefix.clone());
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 4 — for each endpoint file, parse methods and build routes
    // -----------------------------------------------------------------------
    let mut entry_points: Vec<EntryPoint> = Vec::new();

    for (rel_path, _method_syms) in &endpoint_methods_by_file {
        let full = join_path(root, rel_path);
        let src = match read_host_file(&full) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let lines: Vec<&str> = src.lines().collect();
        let n = lines.len();

        // Build a sorted list of (line_idx, method_name, is_public) from the
        // RE_ENDPOINT_METHOD regex over the source (more reliable than LSP lines
        // for body boundary detection).
        struct MethodInfo {
            line_idx: usize, // 0-based line of the signature
            name: String,
            is_public: bool,
        }

        let mut methods: Vec<MethodInfo> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if let Some(caps) = RE_ENDPOINT_METHOD.captures(line) {
                let name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                let is_public = line.contains("public ");
                methods.push(MethodInfo {
                    line_idx: i,
                    name,
                    is_public,
                });
            }
        }
        methods.sort_by_key(|m| m.line_idx);

        // For each method, extract the source lines of its body (from `{` to
        // matching `}`), then look for routes or inner group inside.
        //
        // Body extraction: find the `{` after the signature line, then track
        // brace depth until we reach 0 again.
        let extract_body = |sig_line_idx: usize| -> String {
            // Find start of body: scan forward from sig line for a line containing `{`
            let mut depth = 0i32;
            let mut body_lines: Vec<&str> = Vec::new();
            let mut started = false;
            for idx in sig_line_idx..n.min(sig_line_idx + 200) {
                let l = lines[idx];
                for ch in l.chars() {
                    if ch == '{' {
                        depth += 1;
                        started = true;
                    } else if ch == '}' {
                        depth -= 1;
                    }
                }
                if started {
                    body_lines.push(l);
                }
                if started && depth == 0 {
                    break;
                }
            }
            body_lines.join("\n")
        };

        // Build: private_method_name → Vec<(verb, leaf_route, line_number_1based)>
        let mut leaf_routes: std::collections::HashMap<String, Vec<(String, String, u32)>> =
            std::collections::HashMap::new();

        // Build: public_method_name → inner_group_prefix (may be empty string)
        let mut public_inner_group: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // Build: public_method_name → Vec<private_method_names_chained>
        let mut public_chains: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        for mi in &methods {
            let body = extract_body(mi.line_idx);

            if !mi.is_public {
                // Private leaf method: find app.MapGet/Post/... → verb + path
                let mut routes: Vec<(String, String, u32)> = Vec::new();
                for caps in RE_MAP_ROUTE.captures_iter(&body) {
                    let verb = caps
                        .get(1)
                        .map(|m| m.as_str().to_uppercase())
                        .unwrap_or_default();
                    let path = caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                    // Compute real line number from match start in body → line in file
                    let match_start = caps.get(0).map(|m| m.start()).unwrap_or(0);
                    let lines_before_in_body =
                        body[..match_start].chars().filter(|&c| c == '\n').count();
                    let line_1based = (mi.line_idx + lines_before_in_body + 1) as u32;
                    routes.push((verb, path, line_1based));
                }
                if !routes.is_empty() {
                    leaf_routes.insert(mi.name.clone(), routes);
                }
            } else {
                // Public orchestrator method:
                // - Find inner group prefix via RE_MAP_GROUP
                let inner = RE_MAP_GROUP
                    .captures(&body)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                public_inner_group.insert(mi.name.clone(), inner);

                // - Find chained private method calls: .MapLeaf1().MapLeaf2()...
                let mut chains: Vec<String> = Vec::new();
                for call_cap in RE_ENDPOINT_CALL.captures_iter(&body) {
                    if let Some(name) = call_cap.get(1) {
                        let cname = name.as_str().to_string();
                        // Only collect names that appear as private methods in this file
                        chains.push(cname);
                    }
                }
                public_chains.insert(mi.name.clone(), chains);
            }
        }

        // Now assemble entry points for each public method.
        // Also handle the case where the public method directly contains MapGet/Post
        // (ProblemApi pattern — no private helpers).
        for mi in methods.iter().filter(|m| m.is_public) {
            let outer = outer_prefix.get(&mi.name).cloned().unwrap_or_default();
            let inner = public_inner_group
                .get(&mi.name)
                .cloned()
                .unwrap_or_default();

            let full_prefix = build_route_prefix(&outer, &inner);

            let chains = public_chains.get(&mi.name).cloned().unwrap_or_default();

            // Case A: public method chains private helpers
            let mut found_via_chains = false;
            for private_name in &chains {
                if let Some(routes) = leaf_routes.get(private_name) {
                    for (verb, leaf, line_1based) in routes {
                        let route = combine_prefix_and_leaf(&full_prefix, leaf);
                        let label = format!("{} {}", verb, route);
                        entry_points.push(EntryPoint {
                            label,
                            path: rel_path.clone(),
                            line: Some(*line_1based),
                            method: Some(verb.clone()),
                            description: Some(private_name.clone()),
                            detection_source: Some("lsp".to_string()),
                        });
                        found_via_chains = true;
                    }
                }
            }

            // Case B: public method has direct MapGet/Post (no private helpers)
            if !found_via_chains {
                let body = extract_body(mi.line_idx);
                for caps in RE_MAP_ROUTE.captures_iter(&body) {
                    let verb = caps
                        .get(1)
                        .map(|m| m.as_str().to_uppercase())
                        .unwrap_or_default();
                    let leaf = caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
                    let match_start = caps.get(0).map(|m| m.start()).unwrap_or(0);
                    let lines_before = body[..match_start].chars().filter(|&c| c == '\n').count();
                    let line_1based = (mi.line_idx + lines_before + 1) as u32;

                    let route = combine_prefix_and_leaf(&full_prefix, &leaf);
                    let label = format!("{} {}", verb, route);
                    entry_points.push(EntryPoint {
                        label,
                        path: rel_path.clone(),
                        line: Some(line_1based),
                        method: Some(verb.clone()),
                        description: Some(mi.name.clone()),
                        detection_source: Some("lsp".to_string()),
                    });
                }
            }
        }
    }

    entry_points
}

// Matches `.MethodName(` — used to find chained extension method calls
static RE_ENDPOINT_CALL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\.([A-Z][A-Za-z0-9_]*)\s*\(").unwrap());

/// Build a route prefix from outer and inner group strings.
/// Both may be empty, either, or both present.
fn build_route_prefix(outer: &str, inner: &str) -> String {
    let outer = outer.trim_matches('/');
    let inner = inner.trim_matches('/');
    match (outer.is_empty(), inner.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("/{}", outer),
        (true, false) => format!("/{}", inner),
        (false, false) => format!("/{}/{}", outer, inner),
    }
}

/// Combine a group prefix (e.g. "/v1/outer/inner") with a leaf route (e.g. "/leaf" or "/").
fn combine_prefix_and_leaf(prefix: &str, leaf: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let leaf_trimmed = leaf.trim_start_matches('/');
    if leaf_trimmed.is_empty() || leaf_trimmed == "/" {
        // Leaf is "/" → the route IS just the prefix (trailing slash preserved per ASP.NET)
        if prefix.is_empty() {
            "/".to_string()
        } else {
            format!("{}/", prefix)
        }
    } else {
        format!("{}/{}", prefix, leaf_trimmed)
    }
}

/// Scan a C# controller file for [Route] on the class and [HttpVerb] on methods.
fn scan_controller_routes(rel_path: &str, src: &str) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    // Extract base route (first class-level [Route("...")])
    let base_route = RE_CLASS_ROUTE
        .captures(src)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    let lines: Vec<&str> = src.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = RE_HTTP_METHOD.captures(line) {
            let verb_full = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let verb = verb_full
                .strip_prefix("Http")
                .unwrap_or(verb_full)
                .to_uppercase();
            let fragment = caps.get(2).map(|m| m.as_str()).unwrap_or("").trim();

            let route = build_route(&base_route, fragment);
            let label = format!("{} {}", verb, route);

            // Try to grab the method name from the next non-attribute, non-blank line
            let description = lines
                .get(i + 1)
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('['))
                .and_then(|l| extract_method_name(l))
                .map(|name| name.to_string());

            entry_points.push(EntryPoint {
                label,
                path: rel_path.to_string(),
                line: Some((i + 1) as u32),
                method: Some(verb),
                description,
                detection_source: Some("regex".to_string()),
            });
        }
    }

    entry_points
}

/// Scan a C# file for minimal API route registrations:
/// app.MapGet("/path", ...), app.MapPost(...), etc.
/// Operates on the full file string so multiline calls are matched:
///   _ = app.MapPost(
///       "/validate",
fn scan_minimal_api_routes(rel_path: &str, src: &str) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    for caps in RE_MAP_ROUTE.captures_iter(src) {
        let verb = caps
            .get(1)
            .map(|m| m.as_str().to_uppercase())
            .unwrap_or_default();
        let route = caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
        let label = format!("{} {}", verb, route);

        // Compute 1-based line number from the byte offset of the match start.
        let match_start = caps.get(0).map(|m| m.start()).unwrap_or(0);
        let line_number = src[..match_start].chars().filter(|&c| c == '\n').count() + 1;

        entry_points.push(EntryPoint {
            label,
            path: rel_path.to_string(),
            line: Some(line_number as u32),
            method: Some(verb),
            description: None,
            detection_source: Some("regex".to_string()),
        });
    }

    entry_points
}

// ---------------------------------------------------------------------------
// WebApp (MVC/Pages): controllers + Razor Pages OnGet/OnPost handlers
// ---------------------------------------------------------------------------

fn detect_webapp_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    // Controller actions (same logic as WebAPI)
    let controller_files: Vec<&String> = files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            lower.ends_with("controller.cs")
                || lower.contains("/controllers/")
                || lower.contains("\\controllers\\")
        })
        .collect();

    for rel_path in controller_files {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            let mut eps = scan_controller_routes(rel_path, &src);
            entry_points.append(&mut eps);
        }
    }

    // Razor Pages: *.cshtml.cs files with OnGet/OnPost handlers
    let page_files: Vec<&String> = files
        .iter()
        .filter(|f| f.to_lowercase().ends_with(".cshtml.cs"))
        .collect();

    for rel_path in page_files {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            let mut eps = scan_razor_page_handlers(rel_path, &src);
            entry_points.append(&mut eps);
        }
    }

    entry_points
}

/// Scan a Razor Page code-behind file for OnGet/OnPost/OnPut/OnDelete methods.
fn scan_razor_page_handlers(rel_path: &str, src: &str) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    for (i, line) in src.lines().enumerate() {
        if let Some(caps) = RE_RAZOR_HANDLER.captures(line) {
            let method_name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let verb = caps
                .get(2)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();

            let page_route = rel_path.trim_end_matches(".cshtml.cs").replace('\\', "/");
            let label = format!("{} {} ({})", verb, page_route, method_name);

            entry_points.push(EntryPoint {
                label,
                path: rel_path.to_string(),
                line: Some((i + 1) as u32),
                method: Some(verb),
                description: Some(method_name),
                detection_source: Some("regex".to_string()),
            });
        }
    }

    entry_points
}

// ---------------------------------------------------------------------------
// ConsoleApp: locate the Main() method
// ---------------------------------------------------------------------------

fn detect_console_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    // Prefer Program.cs; otherwise scan all top-level .cs files
    let candidates: Vec<&String> = {
        let program: Vec<&String> = files
            .iter()
            .filter(|f| f.to_lowercase().ends_with("program.cs"))
            .collect();
        if program.is_empty() {
            files.iter().filter(|f| f.ends_with(".cs")).collect()
        } else {
            program
        }
    };

    for rel_path in candidates {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            for (i, line) in src.lines().enumerate() {
                if RE_MAIN.is_match(line) {
                    return vec![EntryPoint {
                        label: "Main()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("MAIN".to_string()),
                        description: Some("Application entry point".to_string()),
                        detection_source: Some("regex".to_string()),
                    }];
                }
            }
        }
    }

    vec![]
}

// ---------------------------------------------------------------------------
// Worker: locate ExecuteAsync() in classes extending BackgroundService
// ---------------------------------------------------------------------------

fn detect_worker_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    for rel_path in files.iter().filter(|f| f.ends_with(".cs")) {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            // Use a simple contains check rather than a single-line regex so that
            // C# 12 primary constructor syntax is handled correctly:
            //
            //   public class MyWorker(ILogger<MyWorker> logger, ...)  // line 1
            //       : BackgroundService                                // separate line
            //
            // A per-line regex would never see both tokens on the same line.
            if !src.contains("BackgroundService") {
                continue;
            }
            for (i, line) in src.lines().enumerate() {
                if RE_EXECUTE_ASYNC.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "ExecuteAsync()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("EXECUTE".to_string()),
                        description: Some("Background worker execution loop".to_string()),
                        detection_source: Some("regex".to_string()),
                    });
                }
            }
        }
    }

    entry_points
}

// ---------------------------------------------------------------------------
// NativeApp (MAUI / WinForms / WPF): locate application startup
// ---------------------------------------------------------------------------

fn detect_native_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    for rel_path in files.iter().filter(|f| f.ends_with(".cs")) {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            for (i, line) in src.lines().enumerate() {
                if RE_CREATE_MAUI_APP.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "CreateMauiApp()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("STARTUP".to_string()),
                        description: Some("MAUI application bootstrap".to_string()),
                        detection_source: Some("regex".to_string()),
                    });
                } else if RE_MAIN.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "Main()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("MAIN".to_string()),
                        description: Some("Application entry point".to_string()),
                        detection_source: Some("regex".to_string()),
                    });
                } else if RE_ON_STARTUP.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "OnStartup()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("STARTUP".to_string()),
                        description: Some("Application startup handler".to_string()),
                        detection_source: Some("regex".to_string()),
                    });
                }
            }
        }
    }

    entry_points
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn join_path(root: &str, file: &str) -> String {
    if root.ends_with('/') {
        format!("{}{}", root, file)
    } else {
        format!("{}/{}", root, file)
    }
}

/// Read the first file in `files` whose bare name matches the predicate.
/// Returns `None` if no matching file is found or all reads fail.
fn read_first_matching_file<F>(root: &str, files: &[String], predicate: F) -> Option<String>
where
    F: Fn(&str) -> bool,
{
    for f in files {
        // Compare only the bare filename (last path component) against the predicate
        let bare = f.rsplit('/').next().unwrap_or(f);
        if predicate(bare) {
            let full = join_path(root, f);
            if let Ok(content) = read_host_file(&full) {
                return Some(content);
            }
        }
    }
    None
}

/// Combine a controller base route with a method-level route fragment.
fn build_route(base: &str, fragment: &str) -> String {
    let base = base.trim_matches('/');
    let fragment = fragment.trim_matches('/');
    if fragment.is_empty() {
        format!("/{}", base)
    } else if fragment.starts_with('/') {
        fragment.to_string()
    } else {
        format!("/{}/{}", base, fragment)
    }
}

/// Given a method signature line, extract the method name (last PascalCase ident before `(`).
fn extract_method_name(line: &str) -> Option<&str> {
    RE_METHOD_NAME.captures(line)?.get(1).map(|m| m.as_str())
}
