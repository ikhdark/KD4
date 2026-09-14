use super::*;
use crate::memory_extensions_root;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn seeds_instructions_without_overwriting_existing_file() {
    let codex_home = TempDir::new().expect("create temp codex home");
    let memory_root = codex_home.path().join("memories");
    let instructions_path = memory_extensions_root(&memory_root).join("ad_hoc/instructions.md");

    seed_instructions(&memory_root)
        .await
        .expect("seed ad-hoc instructions");

    assert_eq!(
        tokio::fs::read_to_string(&instructions_path)
            .await
            .expect("read seeded ad-hoc instructions"),
        INSTRUCTIONS
    );

    tokio::fs::write(&instructions_path, "custom instructions")
        .await
        .expect("write custom instructions");
    seed_instructions(&memory_root)
        .await
        .expect("seed ad-hoc instructions again");

    assert_eq!(
        tokio::fs::read_to_string(&instructions_path)
            .await
            .expect("read custom ad-hoc instructions"),
        "custom instructions"
    );
}

#[tokio::test]
async fn concurrent_seeders_observe_complete_instructions() {
    let home = TempDir::new().unwrap();
    let root = home.path().join("memories");
    let path = memory_extensions_root(&root).join("ad_hoc/instructions.md");
    let seed = || async {
        seed_instructions(&root).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            INSTRUCTIONS
        );
    };
    tokio::join!(seed(), seed(), seed(), seed());
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
}

#[tokio::test]
async fn failed_seeding_does_not_publish_instructions() {
    let home = TempDir::new().unwrap();
    let root = home.path().join("memories");
    let extensions = memory_extensions_root(&root);
    tokio::fs::create_dir_all(&extensions).await.unwrap();
    tokio::fs::write(extensions.join("ad_hoc"), "blocking file")
        .await
        .unwrap();
    assert!(seed_instructions(&root).await.is_err());
    assert!(!extensions.join("ad_hoc/instructions.md").exists());
    assert_eq!(
        tokio::fs::read_to_string(extensions.join("ad_hoc"))
            .await
            .unwrap(),
        "blocking file"
    );
}
