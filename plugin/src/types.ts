// Типы событий и действий ядра — зеркало crates/notesync-core/src/engine/types.rs.
// Ядро отдаёт и принимает их как обычные объекты (serde → JS), байты — Uint8Array.

export interface EngineConfig {
  deviceName?: string;
  excludes?: string[];
  hardExcludes?: string[];
  caseInsensitive?: boolean;
  debounceMs?: number;
  pollActiveMs?: number;
  pollIdleMaxMs?: number;
  useWait?: boolean;
  fullScanMs?: number;
  maxFileSize?: number;
  baseCacheBytes?: number;
  tzOffsetMin?: number;
}

export interface FileMeta {
  path: string;
  size: number;
  mtime: number;
  dir: boolean;
}

export type Expect = { kind: "any" } | { kind: "absent" } | { kind: "stat"; size: number; mtime: number };

export interface HttpRequest {
  method: string;
  path: string;
  headers: [string, string][];
  body: Uint8Array;
  auth: boolean;
  timeoutMs: number;
}

export type SyncState = "idle" | "syncing" | "offline" | "error" | "needPassword" | "blocked";

export interface SyncStatus {
  state: SyncState;
  reason?: string | null;
  pending: number;
  done: number;
  total: number;
  lastSync: number;
  conflicts: number;
  encrypted: boolean;
}

export type LogLevel = "debug" | "info" | "warn" | "error";

export type Notice =
  | { kind: "conflict"; id: number; path: string; copy: string }
  | { kind: "localCopySaved"; path: string; copy: string }
  | { kind: "restoredEdited"; path: string }
  | { kind: "restoredRemote"; path: string }
  | { kind: "followedRename"; from: string; to: string }
  | { kind: "rejected"; path: string; code: string }
  | { kind: "caseCollision"; path: string; existing: string }
  | { kind: "tooLarge"; path: string; size: number }
  | { kind: "wrongPassword" }
  | { kind: "needPassword" }
  | { kind: "encryptionStarted" }
  | { kind: "migrationProgress"; done: number; total: number }
  | { kind: "encryptionEnabled" }
  | { kind: "passwordChanged" }
  | { kind: "unauthorized" }
  | { kind: "protocolUnsupported"; supported: number }
  | { kind: "serverRewound" }
  | { kind: "indexReset" }
  | { kind: "encryptionMismatch" }
  | { kind: "error"; code: string; message: string };

export interface ConflictRecord {
  id: number;
  path: string;
  copy: string;
  at: number;
}

export interface DeletedView {
  path: string;
  deletedAt: number;
  expiresAt: number;
  size: number;
  device: number;
}

export interface RevisionView {
  rev: number;
  size: number;
  mtime: number;
  seq: number;
  device: number;
  deleted: boolean;
  renamedFrom?: string | null;
}

export interface DeviceView {
  id: number;
  name: string;
  createdAt: number;
  lastSeen: number;
  revoked: boolean;
  current: boolean;
}

export type UiResult =
  | { type: "ok" }
  | { type: "error"; code: string; message: string }
  | { type: "deleted"; items: DeletedView[] }
  | { type: "history"; revisions: RevisionView[] }
  | { type: "devices"; vault: string; devices: DeviceView[] }
  | { type: "join"; code: string; url: string; expiresAt: number }
  | { type: "token"; token: string; vault: string; deviceId: number; deviceName: string }
  | { type: "retention"; days: number }
  | {
      type: "stats";
      seq: number;
      files: number;
      folders: number;
      deleted: number;
      liveBytes: number;
      storedBytes: number;
      devices: number;
    }
  | { type: "conflicts"; items: ConflictRecord[] }
  | { type: "strength"; bits: number }
  | { type: "restored"; count: number };

export type UiCommand =
  | { type: "listDeleted" }
  | { type: "restoreDeleted"; paths: string[] }
  | { type: "purgeDeleted"; paths: string[] }
  | { type: "history"; path: string }
  | { type: "restoreRevision"; path: string; rev: number }
  | { type: "devices" }
  | { type: "revokeDevice"; id: number }
  | { type: "createJoin"; name: string }
  | { type: "redeem"; code: string; name: string }
  | { type: "getRetention" }
  | { type: "setRetention"; days: number }
  | { type: "stats" }
  | { type: "conflicts" }
  | { type: "passwordStrength"; password: string };

export type ConflictChoice = "keepBoth" | "keepMine" | "keepServer";

export type IoResult =
  | { type: "http"; status: number; headers: [string, string][]; body: Uint8Array }
  | { type: "listing"; files: FileMeta[] }
  | { type: "stat"; meta: FileMeta | null }
  | { type: "data"; data: Uint8Array }
  | { type: "done" }
  | { type: "notFound" }
  | { type: "precondition" }
  | { type: "failed"; message: string };

export type Action =
  | { type: "http"; id: number; req: HttpRequest }
  | { type: "list"; id: number }
  | { type: "stat"; id: number; path: string }
  | { type: "read"; id: number; path: string; offset: number; len?: number | null }
  | { type: "write"; id: number; path: string; data: Uint8Array; expect: Expect }
  | { type: "writeTemp"; id: number; temp: string; offset: number; data: Uint8Array }
  | { type: "readTemp"; id: number; temp: string; offset: number; len: number }
  | { type: "commitTemp"; id: number; temp: string; path: string; expect: Expect }
  | { type: "deleteTemp"; id: number; temp: string }
  | { type: "trash"; id: number; path: string; expect: Expect }
  | { type: "rename"; id: number; from: string; to: string }
  | { type: "mkdir"; id: number; path: string }
  | { type: "rmdir"; id: number; path: string }
  | { type: "saveIndex"; id: number; data: Uint8Array }
  | { type: "cacheRead"; id: number; key: string }
  | { type: "cacheWrite"; id: number; key: string; data: Uint8Array }
  | { type: "cacheDelete"; id: number; key: string }
  | { type: "wake"; at: number }
  | { type: "status"; status: SyncStatus }
  | { type: "notify"; notice: Notice }
  | { type: "log"; level: LogLevel; message: string }
  | { type: "rememberKey"; key: Uint8Array }
  | { type: "forgetKey" }
  | { type: "uiResult"; req: number; result: UiResult };

/** Действия, на которые ядро ждёт ответ (`done`). */
export type IoAction = Extract<Action, { id: number }>;

export type EngineEvent =
  | { type: "start"; key?: Uint8Array | null }
  | { type: "tick" }
  | { type: "syncNow" }
  | { type: "changed"; path: string }
  | { type: "deleted"; path: string }
  | { type: "renamed"; from: string; to: string }
  | { type: "visible" }
  | { type: "hidden" }
  | { type: "done"; id: number; result: IoResult }
  | { type: "password"; password: string; remember: boolean }
  | { type: "enableEncryption"; password: string; remember: boolean }
  | { type: "changePassword"; old: string; new: string }
  | { type: "resolve"; id: number; choice: ConflictChoice }
  | { type: "command"; req: number; command: UiCommand }
  | { type: "configure"; config: EngineConfig };
