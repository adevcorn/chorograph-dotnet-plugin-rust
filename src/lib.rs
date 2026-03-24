use chorograph_plugin_sdk_rust::prelude::*;
use regex::Regex;

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

    // 2. Build absolute path
    let full_path = join_path(&root, project_file_name);

    // 3. Read the project file content from the host
    let content = match read_host_file(&full_path) {
        Ok(c) => c,
        Err(e) => {
            log!("Failed to read project file {}: {:?}", full_path, e);
            return None;
        }
    };

    // 4. Detect language tag
    let mut tags = vec![".NET".to_string()];
    if project_file_name.ends_with(".csproj") {
        tags.push("C#".to_string());
    } else {
        tags.push("F#".to_string());
    }

    // 5. Detect category
    let category = detect_category(&content);

    // 6. Detect target framework tag
    if let Some(tf) = detect_target_framework(&content) {
        tags.push(tf);
    }

    // 7. Detect entry points based on category
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

fn detect_category(csproj: &str) -> String {
    if csproj.contains("Sdk=\"Microsoft.NET.Sdk.Web\"") {
        if csproj.contains("AddControllersWithViews") || csproj.contains("AddMvc") {
            return "WebApp (MVC/Pages)".to_string();
        } else if csproj.contains("AddControllers") || csproj.contains("MapControllers") {
            return "WebAPI".to_string();
        } else {
            return "WebApp".to_string();
        }
    }
    if csproj.contains("Sdk=\"Microsoft.NET.Sdk.Worker\"") {
        return "Worker".to_string();
    }
    if csproj.contains("<OutputType>Exe</OutputType>") {
        // Distinguish native/MAUI/WinForms/WPF from plain console
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
    // Fast path for common versions
    for ver in &["net9.0", "net8.0", "net7.0", "net6.0"] {
        if csproj.contains(&format!("<TargetFramework>{}</TargetFramework>", ver)) {
            return Some(ver.to_string());
        }
    }
    // Regex fallback
    let re = Regex::new(r"<TargetFramework>(.*?)</TargetFramework>").ok()?;
    let caps = re.captures(csproj)?;
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

    // Controller scan
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

    // Minimal API scan (Program.cs / app.Map*)
    let program_files: Vec<&String> = files
        .iter()
        .filter(|f| f.to_lowercase().ends_with("program.cs"))
        .collect();

    for rel_path in program_files {
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

    // Regex for class-level [Route("...")] — captures the base route template
    let class_route_re =
        Regex::new(r#"\[Route\s*\(\s*"([^"]+)"\s*\)\]"#).unwrap_or_else(|_| unreachable!());

    // Regex for method-level HTTP verb attributes, e.g. [HttpGet("path")] or [HttpGet]
    let method_re = Regex::new(
        r#"\[(Http(?:Get|Post|Put|Delete|Patch|Head|Options))\s*(?:\(\s*"([^"]*)"\s*\))?\]"#,
    )
    .unwrap_or_else(|_| unreachable!());

    // Extract base route (first match on the class)
    let base_route = class_route_re
        .captures(src)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    let lines: Vec<&str> = src.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = method_re.captures(line) {
            let verb_full = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let verb = verb_full
                .strip_prefix("Http")
                .unwrap_or(verb_full)
                .to_uppercase();
            let fragment = caps.get(2).map(|m| m.as_str()).unwrap_or("").trim();

            // Build full route by joining base + fragment
            let route = build_route(&base_route, fragment);
            let label = format!("{} {}", verb, route);

            // Try to grab the method name from the next non-blank line
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

/// Scan Program.cs (or similar) for minimal API route registrations:
/// app.MapGet("/path", ...), app.MapPost(...), etc.
fn scan_minimal_api_routes(rel_path: &str, src: &str) -> Vec<EntryPoint> {
    let mut entry_points = Vec::new();

    let map_re = Regex::new(r#"app\s*\.\s*Map(Get|Post|Put|Delete|Patch)\s*\(\s*"([^"]+)""#)
        .unwrap_or_else(|_| unreachable!());

    for (i, line) in src.lines().enumerate() {
        if let Some(caps) = map_re.captures(line) {
            let verb = caps
                .get(1)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();
            let route = caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
            let label = format!("{} {}", verb, route);

            entry_points.push(EntryPoint {
                label,
                path: rel_path.to_string(),
                line: Some((i + 1) as u32),
                method: Some(verb),
                description: None,
            });
        }
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

    // Razor Pages: look for *.cshtml.cs files with OnGet/OnPost page handler methods
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

    // Matches: public IActionResult OnGet(), public async Task<IActionResult> OnPostAsync(), etc.
    let handler_re =
        Regex::new(r"(?:public|protected)\s+[\w<>\[\]]+\s+(On(Get|Post|Put|Delete|Patch)\w*)\s*\(")
            .unwrap_or_else(|_| unreachable!());

    for (i, line) in src.lines().enumerate() {
        if let Some(caps) = handler_re.captures(line) {
            let method_name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let verb = caps
                .get(2)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();

            // Derive a page route from the file path: strip .cshtml.cs, keep relative segment
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
    // Prefer Program.cs, otherwise scan all .cs files
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

    let main_re = Regex::new(r"static\s+(?:async\s+)?(?:void|Task|int)\s+Main\s*\(")
        .unwrap_or_else(|_| unreachable!());

    for rel_path in candidates {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            for (i, line) in src.lines().enumerate() {
                if main_re.is_match(line) {
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
    let execute_re =
        Regex::new(r"(?:protected|public)\s+override\s+async\s+Task\s+ExecuteAsync\s*\(")
            .unwrap_or_else(|_| unreachable!());

    let bg_service_re = Regex::new(r"class\s+\w+\s*:\s*(?:\w+\s*,\s*)*BackgroundService")
        .unwrap_or_else(|_| unreachable!());

    let mut entry_points = Vec::new();

    for rel_path in files.iter().filter(|f| f.ends_with(".cs")) {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            if !bg_service_re.is_match(&src) {
                continue;
            }
            for (i, line) in src.lines().enumerate() {
                if execute_re.is_match(line) {
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

    // MAUI: CreateMauiApp() in MauiProgram.cs
    let maui_re = Regex::new(r"(?:public|internal)\s+static\s+MauiApp\s+CreateMauiApp\s*\(")
        .unwrap_or_else(|_| unreachable!());

    // WinForms / WPF: Main() or OnStartup() / Application_Startup
    let main_re = Regex::new(r"static\s+(?:async\s+)?(?:void|Task|int)\s+Main\s*\(")
        .unwrap_or_else(|_| unreachable!());

    let startup_re =
        Regex::new(r"(?:protected\s+override\s+void\s+OnStartup|void\s+Application_Startup)\s*\(")
            .unwrap_or_else(|_| unreachable!());

    for rel_path in files.iter().filter(|f| f.ends_with(".cs")) {
        let full = join_path(root, rel_path);
        if let Ok(src) = read_host_file(&full) {
            for (i, line) in src.lines().enumerate() {
                if maui_re.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "CreateMauiApp()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("STARTUP".to_string()),
                        description: Some("MAUI application bootstrap".to_string()),
                    });
                } else if main_re.is_match(line) {
                    entry_points.push(EntryPoint {
                        label: "Main()".to_string(),
                        path: rel_path.to_string(),
                        line: Some((i + 1) as u32),
                        method: Some("MAIN".to_string()),
                        description: Some("Application entry point".to_string()),
                    });
                } else if startup_re.is_match(line) {
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

/// Combine a controller base route with a method-level route fragment.
/// Handles [controller] and [action] tokens and leading-slash rules.
fn build_route(base: &str, fragment: &str) -> String {
    let base = base.trim_matches('/');
    let fragment = fragment.trim_matches('/');
    if fragment.is_empty() {
        format!("/{}", base)
    } else if fragment.starts_with('/') {
        // Absolute override — fragment wins entirely
        fragment.to_string()
    } else {
        format!("/{}/{}", base, fragment)
    }
}

/// Given a line that should contain a method signature, extract the method name.
fn extract_method_name(line: &str) -> Option<&str> {
    // Look for: <modifiers> <return_type> <MethodName>(
    let re = Regex::new(r"\b([A-Z][A-Za-z0-9_]*)\s*\(").ok()?;
    re.captures(line)?.get(1).map(|m| m.as_str())
}
