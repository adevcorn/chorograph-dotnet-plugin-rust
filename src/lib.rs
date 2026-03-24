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

// ---------------------------------------------------------------------------
// Category detection
// ---------------------------------------------------------------------------

fn detect_category(root: &str, files: &[String], csproj: &str) -> String {
    if csproj.contains("Sdk=\"Microsoft.NET.Sdk.Web\"") {
        // The SDK attribute is in the .csproj, but AddControllers/MapControllers
        // live in Program.cs / Startup.cs — so we read those files.
        let program_content = read_first_matching_file(root, files, |name| {
            let lower = name.to_lowercase();
            lower == "program.cs" || lower == "startup.cs"
        });

        if let Some(prog) = program_content {
            if prog.contains("AddControllersWithViews") || prog.contains("AddMvc") {
                return "WebApp (MVC/Pages)".to_string();
            }
            if prog.contains("AddControllers") || prog.contains("MapControllers") {
                return "WebAPI".to_string();
            }
            // Minimal API registrations (app.MapGet/MapPost etc.) → WebAPI
            if RE_MAP_ROUTE.is_match(&prog) {
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
        }

        return "WebApp".to_string();
    }

    if csproj.contains("Sdk=\"Microsoft.NET.Sdk.Worker\"") {
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
// WebAPI: controller route attributes + minimal API (Program.cs)
// ---------------------------------------------------------------------------

fn detect_webapi_entry_points(root: &str, files: &[String]) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    // Controller scan:
    // - Top-level files ending in "controller.cs" (covers shallow listing)
    // - Subdirectory paths containing /controllers/ or \controllers\
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

    // Minimal API scan — search all .cs files for app.Map* registrations.
    // This covers endpoint extension classes (e.g. Endpoints/DrawsEndpoints.cs)
    // where routes are defined outside of Program.cs.
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

    entry_points
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
                    });
                } else if RE_MAIN.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "Main()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("MAIN".to_string()),
                        description: Some("Application entry point".to_string()),
                    });
                } else if RE_ON_STARTUP.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "OnStartup()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("STARTUP".to_string()),
                        description: Some("Application startup handler".to_string()),
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
