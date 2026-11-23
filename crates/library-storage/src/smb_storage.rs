//! SMB читает диапазоны файла напрямую, без загрузки всей записи на диск.
use crate::{Error, NetworkMode, Result};
use smb::{Client, ClientConfig, FileAccessMask, FileCreateArgs, ReadAt, UncPath};
use std::str::FromStr;
pub struct SmbStorage {
    client: Client,
    root: UncPath,
}
pub struct SmbFile {
    file: smb::File,
}
impl SmbStorage {
    pub async fn connect(
        server: &str,
        share: &str,
        user: &str,
        password: String,
        mode: NetworkMode,
    ) -> Result<Self> {
        mode.require_online()?;
        if [server, share]
            .iter()
            .any(|s| s.is_empty() || s.contains(['\\', '/', '\0']))
        {
            return Err(Error::Invalid("неверный адрес SMB".into()));
        }
        let root = UncPath::from_str(&format!("\\\\{server}\\{share}"))
            .map_err(|e| Error::Invalid(format!("{e}")))?;
        let client = Client::new(ClientConfig::default());
        client
            .share_connect(&root, user, password)
            .await
            .map_err(|_| Error::Invalid("SMB-соединение отклонено".into()))?;
        Ok(Self { client, root })
    }
    pub async fn open(&self, path: &str, mode: NetworkMode) -> Result<SmbFile> {
        mode.require_online()?;
        if path.is_empty()
            || path.starts_with(['\\', '/'])
            || path.split(['\\', '/']).any(|p| p == ".." || p.is_empty())
            || path.contains('\0')
        {
            return Err(Error::Invalid("путь вне SMB share".into()));
        }
        let target = self.root.clone().with_path(path);
        let args =
            FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
        let resource = self
            .client
            .create_file(&target, &args)
            .await
            .map_err(|_| Error::NotFound(path.into()))?;
        match resource {
            smb::Resource::File(file) => Ok(SmbFile { file }),
            _ => Err(Error::Invalid("SMB path is not a file".into())),
        }
    }
}
impl SmbFile {
    pub async fn len(&self) -> Result<u64> {
        use smb::GetLen;
        self.file
            .get_len()
            .await
            .map_err(|_| Error::Invalid("SMB metadata failed".into()))
    }
    pub async fn is_empty(&self) -> Result<bool> {
        Ok(self.len().await? == 0)
    }
    pub async fn read_at(&self, offset: u64, max_bytes: usize) -> Result<Vec<u8>> {
        if max_bytes > 4 * 1024 * 1024 {
            return Err(Error::Limit("SMB chunk".into()));
        }
        let mut bytes = vec![0; max_bytes];
        let count = self
            .file
            .read_at(&mut bytes, offset)
            .await
            .map_err(|_| Error::Invalid("SMB read failed".into()))?;
        bytes.truncate(count);
        Ok(bytes)
    }
    pub async fn close(self) -> Result<()> {
        self.file
            .close()
            .await
            .map_err(|_| Error::Invalid("SMB close failed".into()))
    }
}
