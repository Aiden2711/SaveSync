//! End-to-end sync through the Google Drive provider against an in-memory
//! Drive fake. Verifies that metadata queries, multipart uploads, and media
//! downloads compose correctly with `SaveSyncEngine::sync`.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

use savesync_core::google_drive::GoogleDriveProvider;
use savesync_core::http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport};
use savesync_core::oauth::StaticToken;
use savesync_core::{AdapterType, Error, RestoreMode, SaveSyncEngine, SyncProvider};
use serde_json::{Value, json};
use url::Url;

#[derive(Clone)]
struct FakeFile {
    id: String,
    name: String,
    app_properties: HashMap<String, String>,
    content: Vec<u8>,
}

struct FakeDrive {
    files: Mutex<Vec<FakeFile>>,
    next_id: Mutex<u64>,
}

impl FakeDrive {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            files: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
        })
    }

    fn next_id(&self) -> String {
        let mut next = self.next_id.lock().unwrap();
        let id = format!("drive-{}", *next);
        *next += 1;
        id
    }

    fn list(&self, query: &str) -> HttpResponse {
        let filters = [
            ("savesyncKind", extract_filter(query, "savesyncKind")),
            ("savesyncValue", extract_filter(query, "savesyncValue")),
            ("savesyncGameId", extract_filter(query, "savesyncGameId")),
        ];
        let files: Vec<Value> = self
            .files
            .lock()
            .unwrap()
            .iter()
            .filter(|file| {
                filters.iter().all(|(key, expected)| match expected {
                    Some(expected) => file.app_properties.get(*key) == Some(expected),
                    None => true,
                })
            })
            .map(|file| {
                json!({
                    "id": file.id,
                    "name": file.name,
                    "appProperties": file.app_properties,
                })
            })
            .collect();
        json_response(200, json!({ "files": files }))
    }

    fn download(&self, file_id: &str, request: &HttpRequest) -> HttpResponse {
        let files = self.files.lock().unwrap();
        let Some(file) = files.iter().find(|file| file.id == file_id) else {
            return json_response(404, json!({"error":{"message":"not found"}}));
        };
        if let Some(path) = &request.download_to {
            fs::write(path, &file.content).unwrap();
            HttpResponse {
                status: 200,
                headers: vec![],
                body: vec![],
            }
        } else {
            HttpResponse {
                status: 200,
                headers: vec![],
                body: file.content.clone(),
            }
        }
    }

    fn upload(&self, body: &[u8]) -> HttpResponse {
        let (metadata, content) = match parse_multipart(body) {
            Ok(parts) => parts,
            Err(message) => return json_response(400, json!({"error":{"message":message}})),
        };
        let id = self.next_id();
        let name = metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_string();
        let app_properties: HashMap<String, String> = metadata
            .get("appProperties")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        self.files.lock().unwrap().push(FakeFile {
            id: id.clone(),
            name: name.clone(),
            app_properties: app_properties.clone(),
            content,
        });
        json_response(
            200,
            json!({"id": id, "name": name, "appProperties": app_properties}),
        )
    }
}

impl HttpTransport for FakeDrive {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, Error> {
        let url = Url::parse(&request.url)
            .map_err(|error| Error::Network(format!("invalid fake URL: {error}")))?;
        let path = url.path();

        if request.method == HttpMethod::Get && path.ends_with("/files") {
            let query = url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            return Ok(self.list(&query));
        }
        if request.method == HttpMethod::Get && path.contains("/files/") {
            let file_id = path.rsplit('/').next().unwrap_or_default();
            return Ok(self.download(file_id, request));
        }
        if request.method == HttpMethod::Post && path.ends_with("/files") {
            let upload_type = url
                .query_pairs()
                .find(|(key, _)| key == "uploadType")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            if upload_type != "multipart" {
                return Ok(json_response(
                    400,
                    json!({"error":{"message":"fake only supports multipart"}}),
                ));
            }
            let body = request
                .body
                .as_ref()
                .ok_or_else(|| Error::Network("missing upload body".into()))?
                .read_all()?;
            return Ok(self.upload(&body));
        }

        Ok(json_response(
            400,
            json!({"error":{"message":format!("unsupported fake request {} {}", request.method.as_str(), url)}}),
        ))
    }
}

fn json_response(status: u16, body: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

/// Extract `key='value'` from a Drive query string.
fn extract_filter(query: &str, key: &str) -> Option<String> {
    let marker = format!("key='{key}' and value='");
    let start = query.find(&marker)? + marker.len();
    let rest = &query[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

fn parse_multipart(body: &[u8]) -> Result<(Value, Vec<u8>), String> {
    let first_line_end = find(body, b"\r\n").ok_or("missing first boundary line")?;
    let boundary = &body[2..first_line_end];
    let json_start = find(body, b"\r\n\r\n").ok_or("missing JSON headers")? + 4;
    let json_end_marker = [b"\r\n--", boundary].concat();
    let json_end = find(&body[json_start..], &json_end_marker)
        .ok_or("missing JSON/data boundary")?
        + json_start;
    let metadata: Value =
        serde_json::from_slice(&body[json_start..json_end]).map_err(|e| e.to_string())?;

    let data_headers_start = json_end + json_end_marker.len() + 2;
    let data_start = find(&body[data_headers_start..], b"\r\n\r\n")
        .ok_or("missing data headers")?
        + data_headers_start
        + 4;
    let data_end_marker = [b"\r\n--", boundary, b"--"].concat();
    let data_end =
        find(&body[data_start..], &data_end_marker).ok_or("missing final boundary")? + data_start;

    Ok((metadata, body[data_start..data_end].to_vec()))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn provider(drive: Arc<FakeDrive>) -> GoogleDriveProvider {
    GoogleDriveProvider::new(drive, Arc::new(StaticToken::new("fake-token")))
        .with_endpoints(
            "https://drive.test/drive/v3",
            "https://drive.test/upload/drive/v3",
        )
        .with_multipart_limit(u64::MAX)
        .with_retry_policy(savesync_core::http::RetryPolicy {
            max_attempts: 2,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        })
}

#[test]
fn two_devices_sync_through_the_drive_provider() {
    let drive = FakeDrive::new();
    let provider = provider(drive.clone());
    let temp = tempfile::tempdir().unwrap();

    // Device A publishes a snapshot.
    let a_save = temp.path().join("a_save");
    fs::create_dir_all(&a_save).unwrap();
    fs::write(a_save.join("save.bin"), b"from-device-a").unwrap();
    let mut a = SaveSyncEngine::open(temp.path().join("a_data")).unwrap();
    let game = a
        .enroll_game("Drive Game", AdapterType::CustomDirectory)
        .unwrap();
    a.add_directory_location(&game.id, &a_save).unwrap();
    let first = a.snapshot_game(&game.id, Some("first")).unwrap();

    let report = a.sync(&provider, &game.id).unwrap();
    assert_eq!(report.pushed_revisions, 1);
    assert_eq!(report.uploaded_blobs, 1);
    assert_eq!(drive.files.lock().unwrap().len(), 3); // manifest, revision, blob

    // Device B pulls and explicitly restores.
    let b_save = temp.path().join("b_save");
    fs::create_dir_all(&b_save).unwrap();
    let mut b = SaveSyncEngine::open(temp.path().join("b_data")).unwrap();
    b.enroll_game_with_id(&game.id, "Drive Game", AdapterType::CustomDirectory)
        .unwrap();
    b.add_directory_location(&game.id, &b_save).unwrap();

    let report = b.pull(&provider, &game.id).unwrap();
    assert_eq!(report.pulled_revisions, 1);
    assert_eq!(report.downloaded_blobs, 1);
    assert!(fs::read_dir(&b_save).unwrap().next().is_none());

    b.restore(&first.revision_id).unwrap();
    assert_eq!(fs::read(b_save.join("save.bin")).unwrap(), b"from-device-a");

    // B advances, publishes; A pulls and restores.
    fs::write(b_save.join("save.bin"), b"from-device-b").unwrap();
    let second = b.snapshot_game(&game.id, Some("second")).unwrap();
    b.sync(&provider, &game.id).unwrap();

    let report = a.pull(&provider, &game.id).unwrap();
    assert_eq!(report.pulled_revisions, 1);
    let heads = a.heads(&game.id).unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].id, second.revision_id);

    a.restore_with_mode(&second.revision_id, RestoreMode::ExactMirror)
        .unwrap();
    assert_eq!(fs::read(a_save.join("save.bin")).unwrap(), b"from-device-b");
}

#[test]
fn drive_provider_rejects_corrupted_manifest_downloads() {
    let drive = FakeDrive::new();
    let provider = provider(drive.clone());
    let hash = savesync_core::Manifest::empty().hash();

    drive.files.lock().unwrap().push(FakeFile {
        id: "drive-1".into(),
        name: "manifest.json".into(),
        app_properties: HashMap::from([
            ("savesyncKind".to_string(), "manifest".to_string()),
            ("savesyncValue".to_string(), hash.clone()),
        ]),
        content: b"{}".to_vec(),
    });

    assert!(matches!(
        provider.get_manifest(&hash),
        Err(Error::Serialization(_))
    ));
}
