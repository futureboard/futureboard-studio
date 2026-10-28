//! Embeds the Windows manifest (per-monitor DPI awareness, common controls),
//! icon and version info into `SolfegeVoxtune.exe`.

fn main() {
    println!("cargo:rerun-if-changed=windows/voxtune.rc");
    println!("cargo:rerun-if-changed=windows/voxtune.manifest");
    println!("cargo:rerun-if-changed=../../../packages/shared/app/icons/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("windows/voxtune.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
