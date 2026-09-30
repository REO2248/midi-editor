//! Embeds the application icon and a Win32 VERSIONINFO block into
//! midi-editor.exe so Explorer, the installer, and crash reports all see the
//! same product/publisher/version identity. The .rc is generated on the fly
//! so the version always comes from Cargo.toml — never edited by hand.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest.join("assets").join("midi-editor.ico");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());

    let mut fields = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|s| s.parse::<u32>().unwrap_or(0));
    let ver = [
        fields.next().unwrap_or(0),
        fields.next().unwrap_or(0),
        fields.next().unwrap_or(0),
        0,
    ];
    let flags = if std::env::var("PROFILE").as_deref() == Ok("debug") {
        "0x1L" // VS_FF_DEBUG
    } else {
        "0x0L"
    };
    let rc = format!(
        "IDI_ICON1 ICON \"{icon}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION {maj},{min},{pat},{b}\n\
         PRODUCTVERSION {maj},{min},{pat},{b}\n\
         FILEFLAGSMASK 0x3fL\n\
         FILEFLAGS {flags}\n\
         FILEOS 0x40004L\n\
         FILETYPE 0x1L\n\
         FILESUBTYPE 0x0L\n\
         BEGIN\n\
         \x20   BLOCK \"StringFileInfo\"\n\
         \x20   BEGIN\n\
         \x20       BLOCK \"040904b0\"\n\
         \x20       BEGIN\n\
         \x20           VALUE \"CompanyName\", \"{pub_}\"\n\
         \x20           VALUE \"FileDescription\", \"midi-editor - pure-SMF MIDI editor\"\n\
         \x20           VALUE \"FileVersion\", \"{ver}\"\n\
         \x20           VALUE \"InternalName\", \"midi-editor\"\n\
         \x20           VALUE \"LegalCopyright\", \"MIT OR Apache-2.0\"\n\
         \x20           VALUE \"OriginalFilename\", \"midi-editor.exe\"\n\
         \x20           VALUE \"ProductName\", \"midi-editor\"\n\
         \x20           VALUE \"ProductVersion\", \"{ver}\"\n\
         \x20       END\n\
         \x20   END\n\
         \x20   BLOCK \"VarFileInfo\"\n\
         \x20   BEGIN\n\
         \x20       VALUE \"Translation\", 0x409, 1200\n\
         \x20   END\n\
         END\n",
        icon = icon.display().to_string().replace('\\', "/"),
        maj = ver[0],
        min = ver[1],
        pat = ver[2],
        b = ver[3],
        flags = flags,
        pub_ = match env!("CARGO_PKG_AUTHORS").split(':').next().unwrap_or("") {
            "" => "REO2248",
            a => a,
        },
        ver = env!("CARGO_PKG_VERSION"),
    );
    let rc_path = out.join("midi-editor.rc");
    std::fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=PROFILE");
}
