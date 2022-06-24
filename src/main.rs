use anyhow::{bail, Context, Result};
use clap::Parser;
use crypto::{
    aes::KeySize,
    blockmodes,
    buffer::{self, BufferResult, ReadBuffer, WriteBuffer},
};
use lofty::{AudioFile, Probe};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsStr,
    io::{self, Cursor},
    path::{Path, PathBuf},
};
use walkdir::{DirEntry, WalkDir};

static NETEASE_METADATA_AES_KEY: &'static [u8] = "#14ljk_!\\]&0U<'(".as_bytes();

/// 网易云音乐下载文件去重工具
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// 输入媒体文件路径
    #[clap(short, long, value_parser, multiple = true)]
    input: Vec<String>,

    /// 去重后的媒体文件保存到此路径
    #[clap(short, long, value_parser, required = false)]
    output: Option<String>,

    /// 不输出任何文件，仅查看运行结果
    #[clap(short, long, value_parser, default_value_t = false)]
    dry_run: bool,
}

/// 媒体文件信息
#[derive(Debug, Clone)]
struct MediaFileInfo {
    file_path: PathBuf,
    music_id: Option<u64>,
    album: Option<String>,
    track_name: String,
    bitrate: u32,
    duration: u128,
}

impl MediaFileInfo {
    pub fn better_than(&self, other: &MediaFileInfo) -> bool {
        other.bitrate < self.bitrate
            || (other.bitrate == self.bitrate && other.duration < self.duration)
    }
}

/// 网易云音乐标签
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NeteaseKey {
    pub music_id: u64,
}

/// 获取媒体文件信息
fn get_media_file_info<P: AsRef<Path>>(file_path: &P) -> Result<MediaFileInfo> {
    let file_path_buf = PathBuf::from(file_path.as_ref().as_os_str());

    let mut music_id: Option<u64> = None;
    let decryped_data = if file_path_buf.extension().and_then(OsStr::to_str) == Some("ncm") {
        let buffer = std::fs::read(file_path)?;
        println!("decrypting file: {}", file_path_buf.to_str().unwrap());
        // 从 ncm 文件中读取 music id
        if let Ok(ncm_info) = ncmdump::get_info(&buffer) {
            music_id = Some(ncm_info.id);
        }
        Some(ncmdump::convert(&buffer)?)
    } else {
        None
    };

    let tagged_file = if let Some(data) = &decryped_data {
        Probe::new(Cursor::new(data))
            .guess_file_type()?
            .read(true)?
    } else {
        lofty::Probe::open(file_path)?.read(true)?
    };

    let tag = tagged_file
        .primary_tag()
        .unwrap_or(tagged_file.first_tag().with_context(|| "tags not found")?);

    let ncm_key = tag
        .items()
        .into_iter()
        .filter_map(|it| it.value().text())
        .find(|it| it.starts_with("163 key(Don't modify):"))
        .map(|s| s.to_string());

    let album = tag
        .get_texts(&lofty::ItemKey::AlbumTitle)
        .next()
        .map(|s| s.to_string());
    let mut track_name = tag
        .get_texts(&lofty::ItemKey::TrackTitle)
        .next()
        .map(|s| s.to_string());

    if track_name.is_none() {
        // 早期版本的网易云音乐可能会下载到没有标签的音频，据观察，这些文件名一般为歌曲名
        track_name = get_file_name_without_count(&file_path_buf)
            .file_stem()
            .map(|s| s.to_str().unwrap().trim().to_string());
    }

    // 如果有 163 key， 则解密出 music id
    if let Some(key) = &ncm_key {
        let ncm_metadata = decrypt_163_key(key)?;
        music_id = Some(ncm_metadata.music_id);
    }

    Ok(MediaFileInfo {
        file_path: file_path_buf,
        music_id: music_id,
        album: album,
        track_name: track_name.unwrap(),
        bitrate: tagged_file.properties().audio_bitrate().unwrap_or(0),
        duration: tagged_file.properties().duration().as_millis(),
    })
}

/// 解密网易云音乐标签数据
fn decrypt_163_key(key: &str) -> Result<NeteaseKey> {
    if let Some(base64_key) = key.strip_prefix("163 key(Don't modify):") {
        let encrypted_key = base64::decode(base64_key)?;
        let mut final_result = Vec::<u8>::new();
        let mut read_buffer = buffer::RefReadBuffer::new(encrypted_key.as_slice());
        let mut buffer = [0; 4096];
        let mut write_buffer = buffer::RefWriteBuffer::new(&mut buffer);
        let mut decryptor = crypto::aes::ecb_decryptor(
            KeySize::KeySize128,
            NETEASE_METADATA_AES_KEY,
            blockmodes::PkcsPadding,
        );

        loop {
            let result = decryptor.decrypt(&mut read_buffer, &mut write_buffer, true);
            if result.is_ok() {
                final_result.extend(
                    write_buffer
                        .take_read_buffer()
                        .take_remaining()
                        .iter()
                        .map(|&i| i),
                );
            } else {
                break;
            }
            match result.unwrap() {
                BufferResult::BufferUnderflow => break,
                BufferResult::BufferOverflow => {}
            }
        }

        let decrypted_string = String::from_utf8(final_result)?;
        if let Some(ncm_key_json) = decrypted_string.strip_prefix("music:") {
            Ok(serde_json::from_str(&ncm_key_json)?)
        } else {
            bail!("unsupported 163 key: {}", decrypted_string);
        }
    } else {
        bail!("no valid 163 key found");
    }
}

/// 更新 HashMap 中的媒体信息
fn update_media_info(
    id_map: &mut HashMap<u64, MediaFileInfo>,
    without_id_list: &mut Vec<MediaFileInfo>,
    dir_entry: &DirEntry,
) -> Result<()> {
    let file_info = get_media_file_info(&dir_entry.path())?;
    let music_id = file_info.music_id;
    match music_id {
        Some(music_id) => {
            // 有 music id，先去重
            if let Some(old_file_info) = id_map.get(&music_id) {
                println!(
                    "duplicate music id found: \n -- 1. {}\n -- 2. {}",
                    &file_info.file_path.to_str().unwrap(),
                    old_file_info.file_path.to_str().unwrap()
                );
                // 新的文件比特率更高或者比特率相同但时长更长，则替换 map 中的数据
                // 因为网易云音乐会不定期更新一些低音质的文件
                if file_info.better_than(old_file_info) {
                    // 保留码率更高的版本，如果码率一致，保留时长更长的版本
                    id_map.insert(music_id, file_info);
                    println!("    and 1 better than 2");
                }
            } else {
                id_map.insert(music_id, file_info);
            }
        }
        None => {
            // 无 music id，记录下来稍后处理
            without_id_list.push(file_info);
        }
    }
    Ok(())
}

/// 获取不包含 (1) 等计数的文件名
fn get_file_name_without_count(file_path: &PathBuf) -> PathBuf {
    let re = Regex::new(r"\(\d+\)$").unwrap();
    // 可能会爆炸
    let stem = file_path.file_stem().unwrap();
    let result = re.replace_all(stem.to_str().unwrap(), "").to_string();

    match file_path.extension() {
        Some(extension) => {
            PathBuf::from(format!("{}.{}", result.trim(), extension.to_str().unwrap()))
        }
        None => PathBuf::from(result.trim()),
    }
}

/// 给文件名添加 (1) 等计数
fn set_file_name_count(file_name: &PathBuf, count: i32) -> PathBuf {
    // 可能会爆炸
    let stem = file_name.file_stem().unwrap().to_str().unwrap();
    match file_name.extension() {
        Some(extension) => PathBuf::from(format!(
            "{}({}).{}",
            stem,
            count,
            extension.to_str().unwrap()
        )),
        None => PathBuf::from(format!("{}({})", stem, count)),
    }
}

/// 输出文件到目标目录
fn write_out_media_file(
    track_name_map: &HashMap<String, Vec<MediaFileInfo>>,
    output_dir: &PathBuf,
    dry_run: bool,
) {
    track_name_map
        .values()
        .flat_map(|vec| vec.iter())
        .for_each(|file_info| {
            let from_path = &file_info.file_path;
            // 获取文件名并去除计数
            let filename = get_file_name_without_count(from_path);
            let mut output_filename = output_dir.join(&filename);
            let mut count = 0;
            while output_filename.exists() {
                count = count + 1;
                // 如果文件已存在则添加计数
                output_filename = output_dir.join(set_file_name_count(&filename, count));
            }

            println!(
                "copy file from {}\n            to {}",
                &from_path.to_str().unwrap(),
                &output_filename.to_str().unwrap()
            );
            if !dry_run {
                if let Err(e) = std::fs::copy(from_path, output_filename) {
                    eprintln!("{}", e);
                }
            }
        });
}

trait TrackNameMap {
    fn add_media_info(&mut self, media_info: &MediaFileInfo);
    fn is_exists(&self, track_name: &String, album: &String) -> bool;
    fn replace_media_info(&mut self, media_info: &MediaFileInfo);
}

impl TrackNameMap for HashMap<String, Vec<MediaFileInfo>> {
    fn add_media_info(&mut self, media_info: &MediaFileInfo) {
        self.entry(media_info.track_name.clone())
            .or_default()
            .push(media_info.clone());
    }

    fn is_exists(&self, track_name: &String, album: &String) -> bool {
        let inner_vec = self.get(track_name);
        match inner_vec {
            Some(inner_vec) => inner_vec.iter().any(|value| {
                value
                    .album
                    .as_ref()
                    .map_or(false, |album_in_map| album_in_map == album)
            }),
            None => false,
        }
    }

    fn replace_media_info(&mut self, media_info: &MediaFileInfo) {
        let inner_vec = self.entry(media_info.track_name.clone()).or_default();
        // 检查是否已存在相似的音乐
        let mut has_similar = false;
        let mut similar_pos = usize::MAX;
        for (pos, old_media_info) in inner_vec.iter().enumerate() {
            // 对于没有专辑信息的音乐，因为不确定是否为同名音乐，所以判断结果不可靠
            // 可能需要引入音频指纹
            // 如果长度差异在 1.5 秒内，视为相似的音乐
            let has_null_album = old_media_info.album == None || media_info.album == None;
            if old_media_info.album == media_info.album
                || (has_null_album && old_media_info.duration.abs_diff(media_info.duration) < 1500)
            {
                has_similar = true;
                similar_pos = pos;

                println!(
                    "★ probably duplicate music found: \n -- 1. {}\n -- 2. {}",
                    &media_info.file_path.to_str().unwrap(),
                    old_media_info.file_path.to_str().unwrap()
                );
            }
        }
        // 比对当前音乐和相似的音乐哪一个更好
        if has_similar {
            let similar_media_info = inner_vec.get(similar_pos).unwrap();
            if media_info.better_than(similar_media_info) {
                inner_vec.remove(similar_pos);
                inner_vec.push(media_info.clone());
                println!("    and 1 better than 2");
            }
        } else {
            inner_vec.push(media_info.clone());
        }
    }
}

fn main() {
    let cli = Args::parse();

    let output_dir = match cli.output {
        Some(output) => PathBuf::from(output),
        None => PathBuf::from(std::env::current_exe().unwrap())
            .parent()
            .unwrap()
            .to_path_buf()
            .join("nmd-output"),
    };

    if !cli.dry_run {
        match std::fs::create_dir_all(&output_dir) {
            Err(err) => panic!("cannot create output dir: {}", err),
            _ => {}
        }
    }

    println!("scanning files...");

    let extensions: Vec<String> = vec!["wav", "mp3", "flac", "ncm"]
        .into_iter()
        .map(|it| it.to_string())
        .collect();

    // 对于拥有 music id 的媒体文件，根据 music id 先进行去重并保留最佳质量版本
    let mut id_map: HashMap<u64, MediaFileInfo> = HashMap::new();
    // 记录没有 music id 的媒体文件
    let mut without_id_list: Vec<MediaFileInfo> = vec![];

    for input_dir in cli.input {
        let walker = WalkDir::new(input_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok());

        for entry in walker {
            if !entry.file_type().is_file() {
                // 是目录，不是文件
                continue;
            }
            if let Some(ext) = entry.path().extension() {
                let ext = ext.to_str().unwrap().to_lowercase();
                if !extensions.contains(&ext) {
                    // 扩展名不匹配
                    continue;
                }
            } else {
                // 文件没有扩展名
                continue;
            }

            if let Err(e) = update_media_info(&mut id_map, &mut without_id_list, &entry) {
                eprintln!("file: {}, error: {}", entry.path().to_str().unwrap(), e);
            }
        }
    }

    // 然后以歌曲名 + 专辑名去重
    // 比较早的文件只有歌曲名
    println!("Further processing of files without music id...");
    println!("moving data...");
    let mut track_name_map: HashMap<String, Vec<MediaFileInfo>> = HashMap::new();
    // 转存所有已经根据 music id 去重的数据
    id_map
        .values()
        .for_each(|media_info| track_name_map.add_media_info(media_info));
    println!("checking...");
    without_id_list
        .iter()
        .for_each(|media_info| track_name_map.replace_media_info(media_info));

    println!("copy music to output dir? (y/N): ");
    let mut input_string = String::new();
    input_string.clear();
    io::stdin().read_line(&mut input_string).unwrap();
    if input_string.trim().to_lowercase() == "y" {
        write_out_media_file(&track_name_map, &output_dir, cli.dry_run);
    }
}
