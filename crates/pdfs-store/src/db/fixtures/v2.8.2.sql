/* WARNING: Script requires that SQLITE_DBCONFIG_DEFENSIVE be disabled */
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE sync_state (key TEXT PRIMARY KEY, value TEXT);
INSERT INTO sync_state VALUES('schema_version','35');
CREATE TABLE nodes (
  uid           TEXT PRIMARY KEY,
  parent_uid    TEXT,
  name          TEXT NOT NULL,
  is_dir        INTEGER NOT NULL,
  size          INTEGER,
  mtime         INTEGER NOT NULL,
  revision_hash TEXT,
  trashed       INTEGER NOT NULL DEFAULT 0
, node_json TEXT, listed INTEGER NOT NULL DEFAULT 0, path TEXT);
INSERT INTO nodes VALUES('vol~root',NULL,'My Files',1,0,200,NULL,0,'{"uid":{"volume_id":"vol","link_id":"root"},"parent_uid":null,"kind":"Folder","name":"My Files","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',0,'');
INSERT INTO nodes VALUES('vol~docs','vol~root','Docs',1,0,200,NULL,0,'{"uid":{"volume_id":"vol","link_id":"docs"},"parent_uid":{"volume_id":"vol","link_id":"root"},"kind":"Folder","name":"Docs","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',0,'Docs');
INSERT INTO nodes VALUES('vol~report','vol~docs','report.txt',0,10,200,NULL,0,'{"uid":{"volume_id":"vol","link_id":"report"},"parent_uid":{"volume_id":"vol","link_id":"docs"},"kind":{"File":{"media_type":"text/plain","total_size_on_storage":20,"active_revision_state":null,"active_revision_id":null,"claimed_size":10,"claimed_modification_time":null,"content_sha1":null}},"name":"report.txt","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',0,'Docs/report.txt');
INSERT INTO nodes VALUES('vol~old','vol~docs','old.txt',0,3,200,NULL,0,'{"uid":{"volume_id":"vol","link_id":"old"},"parent_uid":{"volume_id":"vol","link_id":"docs"},"kind":{"File":{"media_type":"text/plain","total_size_on_storage":13,"active_revision_state":null,"active_revision_id":null,"claimed_size":3,"claimed_modification_time":null,"content_sha1":null}},"name":"old.txt","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',0,'Docs/old.txt');
INSERT INTO nodes VALUES('vol~shared','vol~docs','shared.txt',0,4,200,NULL,0,'{"uid":{"volume_id":"vol","link_id":"shared"},"parent_uid":{"volume_id":"vol","link_id":"docs"},"kind":{"File":{"media_type":"text/plain","total_size_on_storage":14,"active_revision_state":null,"active_revision_id":null,"claimed_size":4,"claimed_modification_time":null,"content_sha1":null}},"name":"shared.txt","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',0,'Docs/shared.txt');
INSERT INTO nodes VALUES('local~dir','vol~root','Offline folder',1,0,200,NULL,0,'{"uid":{"volume_id":"local","link_id":"dir"},"parent_uid":{"volume_id":"vol","link_id":"root"},"kind":"Folder","name":"Offline folder","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',1,'Offline folder');
INSERT INTO nodes VALUES('local~inner','local~dir','inner.txt',0,5,200,NULL,0,'{"uid":{"volume_id":"local","link_id":"inner"},"parent_uid":{"volume_id":"local","link_id":"dir"},"kind":{"File":{"media_type":"text/plain","total_size_on_storage":15,"active_revision_state":null,"active_revision_id":null,"claimed_size":5,"claimed_modification_time":null,"content_sha1":null}},"name":"inner.txt","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',0,'Offline folder/inner.txt');
INSERT INTO nodes VALUES('local~part','vol~root','movie.part',0,7,200,NULL,0,'{"uid":{"volume_id":"local","link_id":"part"},"parent_uid":{"volume_id":"vol","link_id":"root"},"kind":{"File":{"media_type":"text/plain","total_size_on_storage":17,"active_revision_state":null,"active_revision_id":null,"claimed_size":7,"claimed_modification_time":null,"content_sha1":null}},"name":"movie.part","creation_time":100,"modification_time":200,"trashed":false,"is_shared":false,"is_shared_publicly":false,"signature_email":null,"membership":null,"direct_role":null,"share_id":null,"photo":null,"album":null,"verification":{"name":"NotSigned","passphrase":"NotSigned","content_key":null,"extended_attributes":null}}',0,'movie.part');
CREATE TABLE cache_entries (
  cache_key     TEXT PRIMARY KEY,
  size_bytes    INTEGER,
  last_accessed INTEGER,
  is_pinned     INTEGER NOT NULL DEFAULT 0
, kind TEXT NOT NULL DEFAULT 'blob');
CREATE TABLE pins (
  uid       TEXT PRIMARY KEY,
  path      TEXT NOT NULL,
  recursive INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE local_files (
  id       INTEGER PRIMARY KEY,
  path     TEXT NOT NULL UNIQUE,
  name     TEXT NOT NULL,
  is_dir   INTEGER NOT NULL,
  size     INTEGER NOT NULL DEFAULT 0,
  mtime    INTEGER NOT NULL DEFAULT 0,
  scan_gen INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE photos (
  uid          TEXT PRIMARY KEY,
  capture_time INTEGER NOT NULL,
  name         TEXT,
  ratio        REAL,
  thumb_state  INTEGER NOT NULL DEFAULT 0,
  seq          INTEGER NOT NULL
, media_type TEXT, kind INTEGER NOT NULL DEFAULT 0, favorite INTEGER NOT NULL DEFAULT 0, content_hash TEXT, main_uid TEXT, group_key TEXT, resolved_at INTEGER, latitude REAL, longitude REAL, place_id INTEGER, similar_hash INTEGER);
CREATE TABLE trash (
  uid    TEXT PRIMARY KEY,
  name   TEXT NOT NULL,
  is_dir INTEGER NOT NULL,
  size   INTEGER NOT NULL DEFAULT 0,
  mtime  INTEGER NOT NULL DEFAULT 0
, parent_uid TEXT, trashed_at INTEGER);
CREATE TABLE device (
  uid      TEXT PRIMARY KEY,
  share_id TEXT NOT NULL,
  root_uid TEXT NOT NULL,
  name     TEXT NOT NULL,
  created  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE sync_folder (
  id              INTEGER PRIMARY KEY,
  local_path      TEXT NOT NULL UNIQUE,
  remote_uid      TEXT NOT NULL,
  remote_share_id TEXT NOT NULL,
  mode            TEXT NOT NULL DEFAULT 'mirror',
  state           TEXT NOT NULL DEFAULT 'idle',
  last_sync       INTEGER NOT NULL DEFAULT 0
, pending_mode TEXT);
CREATE TABLE sync_entry (
  folder_id   INTEGER NOT NULL,
  rel_path    TEXT NOT NULL,
  remote_uid  TEXT,
  local_mtime INTEGER NOT NULL DEFAULT 0,
  local_size  INTEGER NOT NULL DEFAULT 0,
  remote_hash TEXT,
  remote_rev  TEXT, local_mtime_ns INTEGER,
  PRIMARY KEY (folder_id, rel_path)
);
CREATE TABLE activity (
  id     INTEGER PRIMARY KEY,
  time   INTEGER NOT NULL,
  kind   TEXT NOT NULL,
  target TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  ok     INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE pending_op (
  id              INTEGER PRIMARY KEY,
  kind            TEXT NOT NULL,
  uid             TEXT NOT NULL,
  blob_path       TEXT,
  meta_json       TEXT,
  created_at      INTEGER NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT,
  next_attempt_at INTEGER NOT NULL DEFAULT 0
, parent_uid TEXT, name TEXT, claimed_at INTEGER NOT NULL DEFAULT 0, access_deferred_since INTEGER NOT NULL DEFAULT 0);
INSERT INTO pending_op VALUES(1,'mkdir','local~dir',NULL,NULL,1700000000000,0,NULL,0,'vol~root','Offline folder',4611686018427387903,0);
INSERT INTO pending_op VALUES(2,'create','local~inner','/blobs/inner','{"len":5,"mtime":1700000000,"based_on":null}',1700000000000,0,NULL,0,'local~dir','inner.txt',0,0);
INSERT INTO pending_op VALUES(3,'create','local~part','/blobs/part','{"len":5,"mtime":1700000000,"based_on":null}',1700000000000,0,NULL,8000000000000,'vol~root','movie.part',0,0);
INSERT INTO pending_op VALUES(4,'revision','vol~report','/blobs/report','{"len":5,"mtime":1700000000,"based_on":null}',1700000000000,2,'network unreachable',1700000900000,NULL,NULL,0,0);
INSERT INTO pending_op VALUES(5,'rename','vol~shared',NULL,'{"original_parent_uid":"vol~docs","original_name":"shared.txt"}',1700000000000,0,NULL,1700000700000,'vol~root','shared-moved.txt',0,1700000100000);
INSERT INTO pending_op VALUES(6,'trash','vol~old',NULL,NULL,1700000000000,0,NULL,0,NULL,NULL,0,0);
CREATE TABLE IF NOT EXISTS 'local_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO local_fts_data VALUES(1,x'000000');
INSERT INTO local_fts_data VALUES(10,x'00000000000000');
CREATE TABLE IF NOT EXISTS 'local_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS 'local_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE IF NOT EXISTS 'local_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO local_fts_config VALUES('version',4);
CREATE TABLE share_access (
  root_uid TEXT PRIMARY KEY,
  access   TEXT NOT NULL
           CHECK (access IN ('owner', 'editor', 'viewer', 'unknown'))
);
CREATE TABLE mount (
  id              INTEGER PRIMARY KEY,
  kind            TEXT NOT NULL
                  CHECK (kind IN ('myfiles', 'device', 'shared')),
  sync_folder_id  INTEGER REFERENCES sync_folder(id) ON DELETE CASCADE,
  share_root_uid  TEXT,
  local_path      TEXT NOT NULL DEFAULT '',
  root_uid        TEXT NOT NULL DEFAULT '',
  root_share_id   TEXT NOT NULL DEFAULT '',
  mode            TEXT NOT NULL DEFAULT 'unknown',
  access          TEXT NOT NULL DEFAULT 'rw'
                  CHECK (access IN ('rw', 'ro')),
  CHECK (
    (kind = 'myfiles' AND sync_folder_id IS NULL AND share_root_uid IS NULL)
    OR
    (kind = 'device' AND sync_folder_id IS NOT NULL AND share_root_uid IS NULL)
    OR
    (kind = 'shared' AND sync_folder_id IS NULL AND share_root_uid IS NOT NULL)
  )
);
CREATE TABLE albums (
  uid           TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  photo_count   INTEGER NOT NULL DEFAULT 0,
  cover_uid     TEXT,
  last_activity INTEGER,
  shared        INTEGER NOT NULL DEFAULT 0,
  seq           INTEGER NOT NULL
);
CREATE TABLE album_photos (
  album_uid    TEXT NOT NULL,
  uid          TEXT NOT NULL,
  capture_time INTEGER NOT NULL,
  name         TEXT,
  media_type   TEXT,
  kind         INTEGER NOT NULL DEFAULT 0,
  ratio        REAL,
  thumb_state  INTEGER NOT NULL DEFAULT 0,
  seq          INTEGER NOT NULL,
  PRIMARY KEY (album_uid, uid)
);
CREATE TABLE IF NOT EXISTS 'nodes_fts_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO nodes_fts_data VALUES(1,x'083850');
INSERT INTO nodes_fts_data VALUES(10,x'000000000108080008010101020101030101040101050101060101070101080101');
INSERT INTO nodes_fts_data VALUES(137438953473,x'000000340430206669010204010366696c0102050103696c6501020601036c657301020701036d79200102020103792066010203040808080808');
INSERT INTO nodes_fts_data VALUES(274877906945,x'0000001a0430646f6302080201010201036f6373020803010103040b');
INSERT INTO nodes_fts_data VALUES(412316860417,x'0000008b04302e747803080801010d01032f72650306010106010363732f03060101040103646f630306010102010365706f03080301010801036f637303060101030202727403080501010a0103706f7203080401010901037265700308020101070202742e03080601010b0103732f7203060101050103742e7403080701010c0202787403080901010e040b0a0a0a0b0a0a0b0b0a0a0b');
INSERT INTO nodes_fts_data VALUES(549755813889,x'0000006b04302e747804080501010a01032f6f6c0406010106010363732f04060101040103642e7404080401010902026f63040601010201036c642e04080301010801036f6373040601010302026c640408020101070103732f6f0406010105010374787404080601010b040b0a0a0b090b0a0a0a');
INSERT INTO nodes_fts_data VALUES(687194767361,x'0000008c04302e747805080801010d01032f736805060101060103617265050804010109010363732f05060101040103642e7405080701010c02026f630506010102010365642e05080601010b010368617205080301010801036f63730506010103010372656405080501010a0103732f73050601010502026861050802010107010374787405080901010e040b0a0b0a0b090b0b0a0b0a0a');
INSERT INTO nodes_fts_data VALUES(824633720833,x'00000084043020666f060809010109010364657206080d01010d0103652066060808010108010366666c06080301010302026c6906080401010402026f6c06080a01010a0103696e6506080601010601036c646506080c01010c0202696e06080501010501036e652006080701010701036f666606080201010202026c6406080b01010b040b0b0b0b0a0a0b0b0a0b0b');
INSERT INTO nodes_fts_data VALUES(962072674305,x'000000da043020666f070601010901032e747807080701011601032f696e07060101100103646572070601010d010365206607060101080202722e07080501011403012f070601010e010366666c070601010302026c69070601010402026f6c070601010a0103696e65070601010603016e07080201011101036c6465070601010c0202696e070601010501036e6520070601010703017207080401011302026e6507080301011201036f6666070601010202026c64070601010b0103722e7407080601011502022f69070601010f0103747874070808010117040a0b0a0a0a0a080a09090a090a090a090a0a090b09');
INSERT INTO nodes_fts_data VALUES(1099511627777,x'0000005c04302e706108080701010701036172740808090101090103652e70080806010106010369652e08080501010501036d6f7608080201010201036f766908080301010301037061720808080101080103766965080804010104040b0b0b0b0b0b0b');
CREATE TABLE IF NOT EXISTS 'nodes_fts_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO nodes_fts_idx VALUES(1,x'',2);
INSERT INTO nodes_fts_idx VALUES(2,x'',2);
INSERT INTO nodes_fts_idx VALUES(3,x'',2);
INSERT INTO nodes_fts_idx VALUES(4,x'',2);
INSERT INTO nodes_fts_idx VALUES(5,x'',2);
INSERT INTO nodes_fts_idx VALUES(6,x'',2);
INSERT INTO nodes_fts_idx VALUES(7,x'',2);
INSERT INTO nodes_fts_idx VALUES(8,x'',2);
CREATE TABLE IF NOT EXISTS 'nodes_fts_content'(id INTEGER PRIMARY KEY, c0, c1);
INSERT INTO nodes_fts_content VALUES(1,'My Files','');
INSERT INTO nodes_fts_content VALUES(2,'Docs','Docs');
INSERT INTO nodes_fts_content VALUES(3,'report.txt','Docs/report.txt');
INSERT INTO nodes_fts_content VALUES(4,'old.txt','Docs/old.txt');
INSERT INTO nodes_fts_content VALUES(5,'shared.txt','Docs/shared.txt');
INSERT INTO nodes_fts_content VALUES(6,'Offline folder','Offline folder');
INSERT INTO nodes_fts_content VALUES(7,'inner.txt','Offline folder/inner.txt');
INSERT INTO nodes_fts_content VALUES(8,'movie.part','movie.part');
CREATE TABLE IF NOT EXISTS 'nodes_fts_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
INSERT INTO nodes_fts_docsize VALUES(1,x'0600');
INSERT INTO nodes_fts_docsize VALUES(2,x'0202');
INSERT INTO nodes_fts_docsize VALUES(3,x'080d');
INSERT INTO nodes_fts_docsize VALUES(4,x'050a');
INSERT INTO nodes_fts_docsize VALUES(5,x'080d');
INSERT INTO nodes_fts_docsize VALUES(6,x'0c0c');
INSERT INTO nodes_fts_docsize VALUES(7,x'0716');
INSERT INTO nodes_fts_docsize VALUES(8,x'0808');
CREATE TABLE IF NOT EXISTS 'nodes_fts_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO nodes_fts_config VALUES('version',4);
CREATE TABLE own_sealed_rev (
  uid         TEXT PRIMARY KEY,
  revision_id TEXT NOT NULL,
  sealed_at   INTEGER NOT NULL
);
INSERT INTO own_sealed_rev VALUES('vol~report','rev-1',1700000000000);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','local_fts','local_fts',0,'CREATE VIRTUAL TABLE local_fts USING fts5(
  name, path, content=''local_files'', content_rowid=''id'', tokenize=''trigram''
)');
INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','nodes_fts','nodes_fts',0,'CREATE VIRTUAL TABLE nodes_fts USING fts5(
  name, path, tokenize=''trigram''
)');
CREATE TRIGGER mount_sync_folder_insert
AFTER INSERT ON sync_folder
BEGIN
  INSERT OR IGNORE INTO mount (kind, sync_folder_id)
  VALUES ('device', NEW.id);
END;
CREATE INDEX idx_nodes_parent ON nodes(parent_uid);
CREATE INDEX idx_photos_seq ON photos(seq);
CREATE INDEX pending_op_uid ON pending_op(uid);
CREATE INDEX pending_op_parent ON pending_op(parent_uid);
CREATE INDEX cache_entries_lru ON cache_entries(kind, last_accessed);
CREATE INDEX idx_nodes_name_nocase ON nodes(name COLLATE NOCASE);
CREATE INDEX idx_local_files_name_nocase
  ON local_files(name COLLATE NOCASE);
CREATE UNIQUE INDEX mount_myfiles
  ON mount(kind) WHERE kind = 'myfiles';
CREATE UNIQUE INDEX mount_device
  ON mount(sync_folder_id) WHERE kind = 'device';
CREATE UNIQUE INDEX mount_shared
  ON mount(share_root_uid) WHERE kind = 'shared';
CREATE INDEX idx_album_photos_seq ON album_photos(album_uid, seq);
CREATE INDEX idx_album_photos_uid ON album_photos(uid);
CREATE INDEX idx_photos_favorite ON photos(favorite, seq);
CREATE INDEX idx_sync_entry_remote_uid ON sync_entry(remote_uid);
CREATE INDEX idx_local_files_scan_gen ON local_files(scan_gen);
CREATE INDEX idx_nodes_parent_live ON nodes(parent_uid) WHERE trashed = 0;
CREATE INDEX idx_pending_op_claimed
    ON pending_op(uid) WHERE claimed_at <> 0;
CREATE INDEX idx_trash_parent ON trash(parent_uid);
CREATE INDEX idx_photos_group ON photos(group_key);
CREATE INDEX idx_photos_unresolved ON photos(resolved_at);
CREATE INDEX idx_photos_place ON photos(place_id);
PRAGMA writable_schema=OFF;
COMMIT;
