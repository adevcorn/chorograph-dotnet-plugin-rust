use chorograph_plugin_sdk_rust::prelude::*;
use regex::Regex;

#[chorograph_plugin]
pub fn init() {
    log!("Dotnet Plugin Loaded");
}

#[chorograph_plugin]
pub fn identify_project(root: String, files: Vec<String>) -> Option<ProjectProfile> {
    // 1. Find project file (.csproj or .fsproj)
    let project_file_name = files.iter().find(|f| f.ends_with(".csproj") || f.ends_with(".fsproj"))?;
    
    // 2. Build absolute path
    let full_path = if root.ends_with('/') {
        format!("{}{}", root, project_file_name)
    } else {
        format!("{}/{}", root, project_file_name)
    };

    // 3. Read the project file content from the host
    let content = match read_host_file(&full_path) {
        Ok(c) => c,
        Err(e) => {
            log!("Failed to read project file {}: {:?}", full_path, e);
            return None;
        }
    };

    // 3. Heuristics
    let mut category = "Library".to_string();
    let mut tags = vec![".NET".to_string()];

    if project_file_name.ends_with(".csproj") {
        tags.push("C#".to_string());
    } else {
        tags.push("F#".to_string());
    }

    // Check for Web SDK
    if content.contains("Sdk=\"Microsoft.NET.Sdk.Web\"") {
        if content.contains("AddControllersWithViews") || content.contains("AddMvc") {
            category = "WebApp (MVC/Pages)".to_string();
        } else if content.contains("AddControllers") || content.contains("MapControllers") {
            category = "WebAPI".to_string();
        } else {
            category = "WebApp".to_string();
        }
    } else if content.contains("Sdk=\"Microsoft.NET.Sdk.Worker\"") {
        category = "Worker".to_string();
    } else if content.contains("<OutputType>Exe</OutputType>") {
        category = "ConsoleApp".to_string();
    }

    // Look for common framework tags
    if content.contains("<TargetFramework>net8.0</TargetFramework>") {
        tags.push("net8.0".to_string());
    } else if content.contains("<TargetFramework>net7.0</TargetFramework>") {
        tags.push("net7.0".to_string());
    } else if let Some(caps) = Regex::new(r"<TargetFramework>(.*?)</TargetFramework>").ok()?.captures(&content) {
        if let Some(m) = caps.get(1) {
            tags.push(m.as_str().to_string());
        }
    }

    Some(ProjectProfile {
        category,
        tags,
    })
}

#[chorograph_plugin]
pub fn handle_action(_action_id: String, _payload: serde_json::Value) {
    // No-op for now
}
