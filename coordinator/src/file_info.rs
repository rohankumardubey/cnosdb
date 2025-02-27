use std::path::PathBuf;

use crypto::{digest::Digest, md5::Md5};
use serde::{Deserialize, Serialize};
use tokio::{fs::File, io::AsyncReadExt};

use crate::errors::CoordinatorResult;

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct FileInfo {
    pub md5: String,
    pub name: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct PathFilesMeta {
    pub path: String,
    pub meta: Vec<FileInfo>,
}

pub async fn get_files_meta(dir: &str) -> CoordinatorResult<PathFilesMeta> {
    let mut files_meta = vec![];
    for name in list_all_filenames(std::path::PathBuf::from(dir)).iter() {
        let meta = get_file_info(name).await?;
        files_meta.push(meta);
    }

    Ok(PathFilesMeta {
        meta: files_meta,
        path: dir.to_string(),
    })
}

pub async fn get_file_info(name: &str) -> CoordinatorResult<FileInfo> {
    let mut file = File::open(name).await?;
    let file_meta = file.metadata().await?;

    let mut md5 = Md5::new();
    let mut buffer = Vec::with_capacity(8 * 1024);
    loop {
        let len = file.read_buf(&mut buffer).await?;
        if len == 0 {
            break;
        }

        md5.input(&buffer[0..len]);
        buffer.clear();
    }

    Ok(FileInfo {
        md5: md5.result_str(),
        name: name.to_string(),
        size: file_meta.len(),
    })
}

fn list_all_filenames(dir: impl AsRef<std::path::Path>) -> Vec<String> {
    let mut list = Vec::new();
    let parent = dir.as_ref().to_string_lossy().to_string();
    for file_name in walkdir::WalkDir::new(dir)
        .min_depth(1)
        .max_depth(5)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| {
            let entry = match e {
                Ok(e) if e.file_type().is_file() => e,
                _ => {
                    return None;
                }
            };

            Some(entry.path().to_string_lossy().to_string())
        })
    {
        list.push(file_name);
    }

    list
}

mod test {
    use crate::file_info::{get_files_meta, list_all_filenames};

    #[tokio::test]
    async fn test_list_filenames() {
        let list = list_all_filenames(std::path::PathBuf::from("../common/".to_string()));
        print!("list_all_filenames: {:#?}", list);

        let files_meta = get_files_meta("../common/").await.unwrap();
        print!("get_files_meta: {:#?}", files_meta);

        let path = "/tmp/cnosdb/test/1/2/3.txt";
        let path = std::path::PathBuf::from(path);

        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .await
            .unwrap();
    }
}
