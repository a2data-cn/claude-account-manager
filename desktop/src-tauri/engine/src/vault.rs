//! 本地加密账号库。与 Python 使用同一 SQLite schema 和 AES-GCM 封装。
use super::store::*;
use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS vault_meta (key TEXT PRIMARY KEY,value BLOB NOT NULL); CREATE TABLE IF NOT EXISTS vault_entries (id TEXT PRIMARY KEY,label TEXT NOT NULL,email TEXT NOT NULL,provider TEXT NOT NULL,account_id TEXT,config_json TEXT NOT NULL,secret BLOB NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);";
const CONFIG_FIELDS: [&str; 6] = [
    "client_id",
    "tenant",
    "imap_host",
    "imap_port",
    "imap_username",
    "notes",
];
static KEYS: OnceLock<Mutex<HashMap<PathBuf, [u8; 32]>>> = OnceLock::new();
fn keys() -> &'static Mutex<HashMap<PathBuf, [u8; 32]>> {
    KEYS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn path(store: &Store) -> PathBuf {
    store.root.join("vault.sqlite3")
}
fn identity(store: &Store) -> PathBuf {
    std::fs::canonicalize(&store.root)
        .unwrap_or_else(|_| store.root.clone())
        .join("vault.sqlite3")
}
fn db_error(_: rusqlite::Error) -> String {
    "账号库读写失败，请检查目录权限或稍后重试".into()
}
fn permissions(store: &Store) {
    set_private(&store.root, true);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        set_private(
            &PathBuf::from(format!("{}{}", path(store).display(), suffix)),
            false,
        );
    }
}
fn connection(store: &Store, create: bool) -> R<Connection> {
    if !create && !path(store).is_file() {
        return Err("尚未创建账号库，请先设置主密码".into());
    }
    if create {
        std::fs::create_dir_all(&store.root).map_err(|_| "无法创建账号库目录")?;
    }
    let db = Connection::open(path(store)).map_err(db_error)?;
    db.busy_timeout(Duration::from_secs(5)).map_err(db_error)?;
    if create {
        db.execute_batch(SCHEMA).map_err(db_error)?;
    }
    permissions(store);
    Ok(db)
}
fn edit_lock(store: &Store, id: &str) -> R<LockGuard> {
    let hash = hex::encode(Sha256::digest(id.as_bytes()));
    try_lock_file(&store.root.join(format!("mailbox-{hash}.lock")))?
        .ok_or_else(|| "这个邮箱正在授权或刷新，暂时不能修改或删除，请稍后重试".into())
}

fn meta(db: &Connection, name: &str) -> R<Option<Vec<u8>>> {
    db.query_row("SELECT value FROM vault_meta WHERE key=?1", [name], |r| {
        r.get(0)
    })
    .optional()
    .map_err(db_error)
}
fn key(store: &Store) -> R<[u8; 32]> {
    keys()
        .lock()
        .map_err(|_| "账号库密钥暂时不可用")?
        .get(&identity(store))
        .copied()
        .ok_or_else(|| "账号库已锁定，请输入主密码解锁".into())
}
fn encrypt(key: &[u8; 32], bytes: &[u8], aad: &[u8]) -> R<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "无效账号库密钥")?;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: bytes, aad })
        .map_err(|_| "无法加密账号库内容")?;
    let mut result = b"CAM1".to_vec();
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&encrypted);
    Ok(result)
}
fn decrypt(key: &[u8; 32], bytes: &[u8], aad: &[u8]) -> R<Vec<u8>> {
    if bytes.len() < 32 || &bytes[..4] != b"CAM1" {
        return Err("账号库密文格式无效".into());
    }
    Aes256Gcm::new_from_slice(key)
        .map_err(|_| "无效账号库密钥")?
        .decrypt(
            Nonce::from_slice(&bytes[4..16]),
            Payload {
                msg: &bytes[16..],
                aad,
            },
        )
        .map_err(|_| "主密码不正确，或账号库内容已损坏".into())
}
fn password(body: &Value) -> R<&str> {
    s(body, "master_password")
        .filter(|p| p.chars().count() >= 8)
        .ok_or_else(|| "主密码至少需要 8 个字符".into())
}
fn derive(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut result = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, 600_000, &mut result);
    result
}

pub fn status(store: &Store) -> R<Value> {
    if !path(store).exists() {
        return Ok(json!({"initialized":false,"unlocked":false,"entries":[]}));
    }
    let db = connection(store, false)?;
    let initialized = meta(&db, "check")?.is_some();
    if !initialized || key(store).is_err() {
        return Ok(json!({"initialized":initialized,"unlocked":false,"entries":[]}));
    }
    let mut stmt = db
        .prepare("SELECT id FROM vault_entries ORDER BY created_at,id")
        .map_err(db_error)?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
    let entries = ids
        .iter()
        .map(|id| get_entry(store, id).map(public_entry))
        .collect::<R<Vec<_>>>()?;
    Ok(json!({"initialized":true,"unlocked":true,"entries":entries}))
}
pub fn setup(store: &Store, body: &Value) -> R<Value> {
    let password = password(body)?;
    let mut db = connection(store, true)?;
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(db_error)?;
    if meta(&tx, "check")?.is_some() {
        return Err("账号库已经初始化，请直接解锁".into());
    }
    let mut salt = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut salt);
    let derived = derive(password, &salt);
    let check = encrypt(&derived, b"cam-vault-v1", b"cam-vault-check")?;
    for (name, value) in [
        ("version", b"1".as_slice()),
        ("salt", salt.as_slice()),
        ("check", check.as_slice()),
    ] {
        tx.execute(
            "INSERT OR REPLACE INTO vault_meta(key,value) VALUES(?1,?2)",
            params![name, value],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    keys()
        .lock()
        .map_err(|_| "无法保存解锁状态")?
        .insert(identity(store), derived);
    permissions(store);
    status(store)
}
pub fn unlock(store: &Store, body: &Value) -> R<Value> {
    let db = connection(store, false)?;
    if meta(&db, "version")?.as_deref() != Some(b"1") {
        return Err("不支持的账号库版本".into());
    }
    let salt = meta(&db, "salt")?
        .filter(|s| s.len() == 32)
        .ok_or("账号库盐值无效")?;
    let check = meta(&db, "check")?.ok_or("账号库尚未初始化")?;
    let mut derived = derive(password(body)?, &salt);
    if decrypt(&derived, &check, b"cam-vault-check")? != b"cam-vault-v1" {
        derived.fill(0);
        return Err("主密码不正确".into());
    }
    keys()
        .lock()
        .map_err(|_| "无法保存解锁状态")?
        .insert(identity(store), derived);
    status(store)
}
pub fn lock(store: &Store) -> R<Value> {
    {
        let mut cached = keys().lock().map_err(|_| "无法锁定账号库")?;
        let identity = identity(store);
        if let Some(k) = cached.get_mut(&identity) {
            k.fill(0);
            std::hint::black_box(k);
        }
        cached.remove(&identity);
    }
    status(store)
}
fn validate_email(email: &str) -> R<String> {
    let email = email.trim();
    if email.len() > 320
        || email.chars().any(char::is_whitespace)
        || email.matches('@').count() != 1
        || email.starts_with('@')
        || email.ends_with('@')
    {
        return Err("请输入有效邮箱地址".into());
    }
    Ok(email.to_string())
}
fn public_entry(mut private: Value) -> Value {
    let secret = private
        .as_object_mut()
        .unwrap()
        .remove("secret")
        .unwrap_or(Value::Null);
    let present = |name: &str| s(&secret, name).is_some();
    private["has_password"] = json!(present("password"));
    private["has_client_secret"] = json!(present("client_secret"));
    private["connected"] = json!(match s(&private, "provider") {
        Some("gmail" | "outlook") => present("refresh_token") || present("access_token"),
        Some("imap") => present("password"),
        _ => false,
    });
    private
}
pub fn get_entry(store: &Store, id: &str) -> R<Value> {
    let k = key(store)?;
    let db = connection(store, false)?;
    let row = db.query_row("SELECT label,email,provider,account_id,config_json,secret,created_at,updated_at FROM vault_entries WHERE id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,String>(4)?,r.get::<_,Vec<u8>>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?))).optional().map_err(db_error)?.ok_or("账号库条目不存在")?;
    let secret: Value = serde_json::from_slice(&decrypt(
        &k,
        &row.5,
        format!("cam-vault-entry:{id}").as_bytes(),
    )?)
    .map_err(|_| "账号库秘密内容损坏")?;
    if !secret.is_object() {
        return Err("账号库秘密格式无效".into());
    }
    let config: Value = serde_json::from_str(&row.4).map_err(|_| "账号库配置损坏")?;
    Ok(
        json!({"id":id,"label":row.0,"email":row.1,"provider":row.2,"account_id":row.3,"config":config,"secret":secret,"created_at":row.6,"updated_at":row.7}),
    )
}
pub fn save_entry(store: &Store, body: &Value) -> R<Value> {
    let k = key(store)?;
    let id = s(body, "id")
        .map(String::from)
        .unwrap_or_else(|| format!("{}{}", new_id(), new_id()));
    let _operation = edit_lock(store, &id)?;
    let old = if s(body, "id").is_some() {
        get_entry(store, &id)?
    } else {
        json!({"config":{},"secret":{},"created_at":now_iso()})
    };
    let field = |name: &str| s(body, name).or_else(|| s(&old, name));
    let email = validate_email(field("email").ok_or("请填写邮箱")?)?;
    let provider = field("provider").unwrap_or("credential");
    if !["credential", "gmail", "outlook", "imap"].contains(&provider) {
        return Err("不支持的邮箱类型".into());
    }
    let label: String = field("label")
        .unwrap_or(&email)
        .trim()
        .chars()
        .take(100)
        .collect();
    let mut config = old["config"].as_object().cloned().unwrap_or_default();
    if let Some(c) = body.get("config") {
        let c = c.as_object().ok_or("邮箱配置应为对象")?;
        for (name, value) in c {
            if !CONFIG_FIELDS.contains(&name.as_str()) {
                return Err(format!("不允许的公开配置字段：{name}"));
            }
            if name == "imap_port" {
                if !value.is_null() && !value.as_u64().is_some_and(|p| p > 0 && p <= 65535) {
                    return Err("IMAP 端口应为 1–65535".into());
                }
            } else if !value.is_null() && !value.is_string() {
                return Err(format!("{name} 应为文字"));
            }
            if value.is_null() {
                config.remove(name);
            } else {
                config.insert(name.clone(), value.clone());
            }
        }
    }
    let mut secret = old["secret"].as_object().cloned().unwrap_or_default();
    if s(&old, "email").is_some_and(|e| !e.eq_ignore_ascii_case(&email))
        || s(&old, "provider").is_some_and(|p| p != provider)
        || ["client_id", "tenant"]
            .iter()
            .any(|name| old["config"].get(*name) != config.get(*name))
        || s(body, "client_secret")
            .filter(|s| !s.is_empty())
            .is_some_and(|secret| Some(secret) != s(&old["secret"], "client_secret"))
    {
        for name in ["access_token", "refresh_token", "expires_at"] {
            secret.remove(name);
        }
    }
    for name in ["password", "client_secret"] {
        if let Some(v) = body.get(name) {
            let v = v.as_str().ok_or_else(|| format!("{name} 应为文字"))?;
            if !v.is_empty() {
                secret.insert(name.into(), json!(v));
            }
        }
    }
    if body.get("clear_password").and_then(Value::as_bool) == Some(true) {
        secret.remove("password");
    }
    let account_id = body.get("account_id").unwrap_or(&old["account_id"]);
    if !account_id.is_null() && !account_id.is_string() {
        return Err("account_id 应为文字".into());
    }
    let encrypted = encrypt(
        &k,
        &serde_json::to_vec(&secret).map_err(|_| "无法编码秘密")?,
        format!("cam-vault-entry:{id}").as_bytes(),
    )?;
    let db = connection(store, false)?;
    db.execute("INSERT INTO vault_entries(id,label,email,provider,account_id,config_json,secret,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(id) DO UPDATE SET label=excluded.label,email=excluded.email,provider=excluded.provider,account_id=excluded.account_id,config_json=excluded.config_json,secret=excluded.secret,updated_at=excluded.updated_at",params![id,label,email,provider,account_id.as_str(),Value::Object(config).to_string(),encrypted,s(&old,"created_at").unwrap_or_default(),now_iso()]).map_err(db_error)?;
    permissions(store);
    status(store)
}
pub fn delete_entry(store: &Store, id: &str) -> R<Value> {
    let _ = key(store)?;
    let _operation = edit_lock(store, id)?;
    get_entry(store, id)?;
    let db = connection(store, false)?;
    db.execute("DELETE FROM vault_entries WHERE id=?1", [id])
        .map_err(db_error)?;
    status(store)
}
pub fn reveal_entry(store: &Store, id: &str) -> R<Value> {
    let entry = get_entry(store, id)?;
    Ok(json!({"password":entry["secret"].get("password").cloned().unwrap_or_else(|| json!(""))}))
}
pub fn save_secret(store: &Store, id: &str, secret: &Value) -> R<()> {
    let k = key(store)?;
    let entry = get_entry(store, id)?;
    let changes = secret.as_object().ok_or("秘密应为对象")?;
    let mut merged = entry["secret"].as_object().cloned().unwrap_or_default();
    for (name, value) in changes {
        if value.is_null() {
            merged.remove(name);
        } else {
            merged.insert(name.clone(), value.clone());
        }
    }
    let encrypted = encrypt(
        &k,
        &serde_json::to_vec(&merged).map_err(|_| "无法编码秘密")?,
        format!("cam-vault-entry:{id}").as_bytes(),
    )?;
    let db = connection(store, false)?;
    if db
        .execute(
            "UPDATE vault_entries SET secret=?1,updated_at=?2 WHERE id=?3",
            params![encrypted, now_iso(), id],
        )
        .map_err(db_error)?
        == 0
    {
        return Err("账号库条目不存在".into());
    }
    permissions(store);
    Ok(())
}
pub fn set_email(store: &Store, id: &str, email: &str) -> R<()> {
    save_entry(store, &json!({"id":id,"email":email}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Store {
        Store {
            root: std::env::temp_dir().join(format!(
                "cam-vault-test-{}-{}",
                std::process::id(),
                new_id()
            )),
        }
    }
    #[test]
    fn python_crypto_fixture_decrypts_with_the_same_master_key() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../../tests/shared/vault_crypto.json"))
                .unwrap();
        let salt = hex::decode(fixture["salt_hex"].as_str().unwrap()).unwrap();
        let key = derive(fixture["master_password"].as_str().unwrap(), &salt);
        assert_eq!(
            hex::encode(key),
            fixture["derived_key_hex"].as_str().unwrap()
        );
        assert_eq!(
            decrypt(
                &key,
                &hex::decode(fixture["check_hex"].as_str().unwrap()).unwrap(),
                b"cam-vault-check"
            )
            .unwrap(),
            b"cam-vault-v1"
        );
        let aad = format!("cam-vault-entry:{}", fixture["entry_id"].as_str().unwrap());
        let secret: Value = serde_json::from_slice(
            &decrypt(
                &key,
                &hex::decode(fixture["entry_blob_hex"].as_str().unwrap()).unwrap(),
                aad.as_bytes(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(secret, fixture["entry_secret"]);
    }

    #[test]
    fn encrypted_crud_locking_and_tamper_detection() {
        let store = fixture();
        assert!(!path(&store).exists());
        assert_eq!(status(&store).unwrap()["initialized"], false);
        assert!(!path(&store).exists());
        setup(&store, &json!({"master_password":"master-123"})).unwrap();
        let st=save_entry(&store,&json!({"label":"Work","email":"a@example.com","provider":"imap","password":"never-write-plaintext","config":{"imap_host":"mail.example.com"}})).unwrap();
        let id = st["entries"][0]["id"].as_str().unwrap();
        assert!(!st.to_string().contains("never-write"));
        assert_eq!(
            reveal_entry(&store, id).unwrap()["password"],
            "never-write-plaintext"
        );
        assert!(!std::fs::read(path(&store))
            .unwrap()
            .windows(b"never-write-plaintext".len())
            .any(|w| w == b"never-write-plaintext"));
        save_entry(&store, &json!({"id":id,"password":"","label":"renamed"})).unwrap();
        assert_eq!(
            reveal_entry(&store, id).unwrap()["password"],
            "never-write-plaintext"
        );
        lock(&store).unwrap();
        assert!(get_entry(&store, id).is_err());
        assert!(unlock(&store, &json!({"master_password":"wrong-password"})).is_err());
        unlock(&store, &json!({"master_password":"master-123"})).unwrap();
        let db = connection(&store, false).unwrap();
        db.execute(
            "UPDATE vault_entries SET secret=?1 WHERE id=?2",
            params![b"CAM1broken".as_slice(), id],
        )
        .unwrap();
        assert!(get_entry(&store, id).is_err());
        let _ = lock(&store);
        std::fs::remove_dir_all(store.root).unwrap();
    }
    #[test]
    fn cipher_uses_bound_aad_and_nonce_and_rejects_config_secrets() {
        let k = [7u8; 32];
        let a = encrypt(&k, b"secret", b"entry-a").unwrap();
        let b = encrypt(&k, b"secret", b"entry-a").unwrap();
        assert_ne!(a, b);
        assert_eq!(decrypt(&k, &a, b"entry-a").unwrap(), b"secret");
        assert!(decrypt(&k, &a, b"entry-b").is_err());
        let store = fixture();
        setup(&store, &json!({"master_password":"master-123"})).unwrap();
        assert!(save_entry(
            &store,
            &json!({"email":"a@example.com","config":{"password":"bad"}})
        )
        .is_err());
        let st=save_entry(&store,&json!({"email":"a@example.com","provider":"gmail","client_secret":"old-client-secret","config":{"client_id":"client-one"}})).unwrap();
        let id = st["entries"][0]["id"].as_str().unwrap();
        save_secret(
            &store,
            id,
            &json!({"access_token":"old","refresh_token":"refresh","expires_at":9999999999i64}),
        )
        .unwrap();
        let changed = save_entry(
            &store,
            &json!({"id":id,"client_secret":"new-client-secret"}),
        )
        .unwrap();
        assert_eq!(changed["entries"][0]["connected"], false);
        assert!(get_entry(&store, id).unwrap()["secret"]
            .get("refresh_token")
            .is_none());
        let guard = edit_lock(&store, id).unwrap();
        assert!(save_entry(&store, &json!({"id":id,"label":"busy"}))
            .unwrap_err()
            .contains("正在授权"));
        assert!(delete_entry(&store, id).is_err());
        drop(guard);
        assert!(delete_entry(&store, id).unwrap()["entries"]
            .as_array()
            .unwrap()
            .is_empty());
        let _ = lock(&store);
        std::fs::remove_dir_all(store.root).unwrap();
    }
}
