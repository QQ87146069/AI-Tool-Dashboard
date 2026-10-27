//! User-scoped login startup. Registration changes only after the user toggles the setting.
use std::{
    io,
    path::{Path, PathBuf},
};

pub struct Startup {
    exe: PathBuf,
    state: PathBuf,
}

impl Startup {
    pub fn new(state: &Path) -> io::Result<Self> {
        Ok(Self {
            exe: std::env::current_exe()?,
            state: std::path::absolute(state)?,
        })
    }

    pub fn enabled(&self) -> io::Result<bool> {
        Ok(platform::read()?.is_some_and(|value| self.entry().is_ok_and(|entry| value == entry)))
    }

    pub fn set_enabled(&self, enabled: bool) -> io::Result<()> {
        let entry = enabled.then(|| self.entry()).transpose()?;
        platform::write(entry.as_deref())
    }

    fn entry(&self) -> io::Result<String> {
        let exe = path_text(&self.exe)?;
        let state = path_text(&self.state)?;
        #[cfg(target_os = "windows")]
        {
            windows_command(exe, state)
        }
        #[cfg(target_os = "macos")]
        {
            Ok(macos_agent(exe, state))
        }
        #[cfg(target_os = "linux")]
        {
            linux_entry(exe, state)
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            let _ = (exe, state);
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

fn path_text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .filter(|text| !text.chars().any(char::is_control))
        .ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

#[cfg(any(target_os = "windows", test))]
fn windows_command(exe: &str, state: &str) -> io::Result<String> {
    if [exe, state]
        .iter()
        .any(|s| s.contains('"') || s.ends_with('\\'))
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let command = format!("\"{exe}\" --background --state \"{state}\"");
    if command.encode_utf16().count() > 260 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(command)
}

#[cfg(any(target_os = "macos", test))]
fn macos_agent(exe: &str, state: &str) -> String {
    let xml = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>io.aitooldashboard.app</string>
<key>ProgramArguments</key><array><string>{}</string><string>--background</string><string>--state</string><string>{}</string></array>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>
"#,
        xml(exe),
        xml(state)
    )
}

#[cfg(any(target_os = "linux", test))]
fn linux_entry(exe: &str, state: &str) -> io::Result<String> {
    if exe.contains('=') {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // First quote Exec arguments, then escape the desktop file's string value layer.
    let quote = |s: &str| {
        let s = s.replace('%', "%%");
        let mut result = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                result.push('\\');
            }
            result.push(c);
        }
        result.push('"');
        result.replace('\\', "\\\\")
    };
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=AI Tool Dashboard\nExec={} --background --state {}\nTerminal=false\n",
        quote(exe),
        quote(state)
    ))
}

#[cfg(target_os = "windows")]
mod platform {
    use std::io;
    use winreg::{HKCU, enums::KEY_SET_VALUE};
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "AI Tool Dashboard";

    pub fn read() -> io::Result<Option<String>> {
        let value = HKCU.open_subkey(RUN).and_then(|key| key.get_value(NAME));
        match value {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn write(entry: Option<&str>) -> io::Result<()> {
        if let Some(entry) = entry {
            HKCU.create_subkey(RUN)?.0.set_value(NAME, &entry)
        } else {
            match HKCU
                .open_subkey_with_flags(RUN, KEY_SET_VALUE)
                .and_then(|key| key.delete_value(NAME))
            {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod platform {
    use std::{fs, io, path::PathBuf};
    fn path() -> io::Result<PathBuf> {
        let dirs = directories::BaseDirs::new().ok_or(io::ErrorKind::NotFound)?;
        #[cfg(target_os = "macos")]
        {
            Ok(dirs
                .home_dir()
                .join("Library/LaunchAgents/io.aitooldashboard.app.plist"))
        }
        #[cfg(target_os = "linux")]
        {
            Ok(dirs
                .config_dir()
                .join("autostart/io.aitooldashboard.app.desktop"))
        }
    }
    pub fn read() -> io::Result<Option<String>> {
        match fs::read_to_string(path()?) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub fn write(entry: Option<&str>) -> io::Result<()> {
        let path = path()?;
        if let Some(entry) = entry {
            fs::create_dir_all(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
            fs::write(path, entry)
        } else {
            match fs::remove_file(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            }
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod platform {
    use std::io;
    pub fn read() -> io::Result<Option<String>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn write(_: Option<&str>) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_preserves_spaced_paths_and_background_state_arguments() {
        assert_eq!(
            windows_command(r"C:\My App\看板.exe", r"C:\My Data\window.json").unwrap(),
            "\"C:\\My App\\看板.exe\" --background --state \"C:\\My Data\\window.json\""
        );
        assert!(windows_command(&"x".repeat(260), "state").is_err());
        assert!(windows_command("bad\"path", "state").is_err());
        let xml = macos_agent("/A&B/App", "/data/<state>.json");
        assert!(xml.contains("/A&amp;B/App"));
        assert!(xml.contains("/data/&lt;state&gt;.json"));
        assert!(xml.contains("<key>RunAtLoad</key><true/>"));
        let desktop = linux_entry("/My App/run", "/data/$a`b\\c%p\"d").unwrap();
        assert!(desktop.contains("Exec=\"/My App/run\" --background --state"));
        assert!(desktop.contains("\\\\$a\\\\`b\\\\\\\\c%%p\\\\\"d"));
        assert!(linux_entry("/bad=exe", "/state").is_err());
    }
}
