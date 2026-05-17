use super::*;

#[derive(Debug, Clone)]
struct WebFilePickerEntry {
    hidden: bool,
    kind: WebFilePickerEntryKind,
    name: String,
    path: String,
    size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WebFilePickerMode {
    Directory,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebFilePickerEntryKind {
    Directory,
    File,
}

impl WebFilePickerMode {
    fn from_message(message: &Value) -> Self {
        match message.get("mode").and_then(Value::as_str) {
            Some("file") => Self::File,
            _ => Self::Directory,
        }
    }
}

impl WebFilePickerEntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::Directory => 0,
            Self::File => 1,
        }
    }
}

pub(crate) fn is_web_file_picker_message(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some(WEB_FILE_PICKER_LIST_MESSAGE)
}

pub(crate) fn dispatch_web_file_picker_message(message: Value) -> Result<Value, String> {
    match message.get("type").and_then(Value::as_str) {
        Some(WEB_FILE_PICKER_LIST_MESSAGE) => {
            let path = message.get("path").and_then(Value::as_str);
            let mode = WebFilePickerMode::from_message(&message);
            let images_only = message
                .get("imagesOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(json!({
                "messages": [],
                "value": web_file_picker_payload(path, mode, images_only)?,
            }))
        }
        Some(message_type) => Err(format!(
            "unsupported web file picker request: {}",
            message_type
        )),
        None => Err("missing web file picker request type".to_string()),
    }
}

#[cfg(test)]
pub(super) fn web_file_picker_directory_payload(path: Option<&str>) -> Result<Value, String> {
    web_file_picker_payload(path, WebFilePickerMode::Directory, false)
}

pub(super) fn web_file_picker_payload(
    path: Option<&str>,
    mode: WebFilePickerMode,
    images_only: bool,
) -> Result<Value, String> {
    let directory = normalize_web_file_picker_path(path)?;
    let metadata = fs::metadata(&directory)
        .map_err(|err| format!("cannot open {}: {}", directory.display(), err))?;
    if !metadata.is_dir() {
        return Err(format!("not a directory: {}", directory.display()));
    }

    let mut entries = Vec::new();
    let read_dir = fs::read_dir(&directory)
        .map_err(|err| format!("cannot read {}: {}", directory.display(), err))?;
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some((kind, size_bytes)) = web_file_picker_entry_kind(&entry) else {
            continue;
        };
        if mode == WebFilePickerMode::Directory && kind != WebFilePickerEntryKind::Directory {
            continue;
        }
        if mode == WebFilePickerMode::File
            && kind == WebFilePickerEntryKind::File
            && images_only
            && !web_file_picker_path_is_image(&entry.path())
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        entries.push(WebFilePickerEntry {
            hidden: name.starts_with('.'),
            kind,
            name,
            path: entry.path().to_string_lossy().into_owned(),
            size_bytes,
        });
    }

    entries.sort_by(|a, b| {
        a.kind.sort_rank().cmp(&b.kind.sort_rank()).then_with(|| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        })
    });
    let truncated = entries.len() > WEB_FILE_PICKER_ENTRY_LIMIT;
    entries.truncate(WEB_FILE_PICKER_ENTRY_LIMIT);

    let entries = entries
        .into_iter()
        .map(|entry| {
            let mut value = json!({
                "hidden": entry.hidden,
                "kind": entry.kind.as_str(),
                "name": entry.name,
                "path": entry.path,
            });
            if let Some(size_bytes) = entry.size_bytes {
                value["sizeBytes"] = json!(size_bytes);
            }
            value
        })
        .collect::<Vec<_>>();
    let parent = directory.parent().map(path_to_string);

    Ok(json!({
        "entries": entries,
        "parent": parent,
        "path": path_to_string(&directory),
        "truncated": truncated,
    }))
}

fn web_file_picker_entry_kind(
    entry: &fs::DirEntry,
) -> Option<(WebFilePickerEntryKind, Option<u64>)> {
    match entry.file_type() {
        Ok(file_type) if file_type.is_dir() => Some((WebFilePickerEntryKind::Directory, None)),
        Ok(file_type) if file_type.is_file() => {
            let size_bytes = entry.metadata().ok().map(|metadata| metadata.len());
            Some((WebFilePickerEntryKind::File, size_bytes))
        }
        Ok(file_type) if file_type.is_symlink() => {
            let metadata = entry.metadata().ok()?;
            if metadata.is_dir() {
                Some((WebFilePickerEntryKind::Directory, None))
            } else if metadata.is_file() {
                Some((WebFilePickerEntryKind::File, Some(metadata.len())))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn web_file_picker_path_is_image(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "avif"
            | "bmp"
            | "gif"
            | "heic"
            | "heif"
            | "jpeg"
            | "jpg"
            | "png"
            | "svg"
            | "tif"
            | "tiff"
            | "webp"
    )
}

fn normalize_web_file_picker_path(path: Option<&str>) -> Result<PathBuf, String> {
    let trimmed = path.unwrap_or("").trim();
    let mut directory = if trimmed.is_empty() {
        default_web_file_picker_path()
    } else if trimmed == "~" {
        home_directory().unwrap_or_else(default_web_file_picker_path)
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        match home_directory() {
            Some(home) => home.join(rest),
            None => PathBuf::from(trimmed),
        }
    } else {
        PathBuf::from(trimmed)
    };

    if let Ok(canonical) = fs::canonicalize(&directory) {
        directory = canonical;
    }
    Ok(directory)
}

fn default_web_file_picker_path() -> PathBuf {
    home_directory().unwrap_or_else(|| {
        if cfg!(windows) {
            PathBuf::from(r"C:\")
        } else {
            PathBuf::from("/")
        }
    })
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
