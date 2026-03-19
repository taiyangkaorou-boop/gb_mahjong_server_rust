BEGIN;

CREATE TABLE user_sessions (
  session_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
  user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
  session_token_hash TEXT NOT NULL UNIQUE,
  issued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at TIMESTAMPTZ NOT NULL,
  revoked_at TIMESTAMPTZ,
  last_seen_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT user_sessions_expiry_valid
    CHECK (expires_at > issued_at),
  CONSTRAINT user_sessions_revoked_after_issue
    CHECK (revoked_at IS NULL OR revoked_at >= issued_at)
);

CREATE TABLE rooms (
  room_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
  room_code TEXT NOT NULL UNIQUE,
  owner_user_id TEXT NOT NULL REFERENCES users(user_id),
  status TEXT NOT NULL,
  config_snapshot JSONB NOT NULL,
  current_match_id TEXT REFERENCES matches(match_id),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT rooms_status_valid
    CHECK (status IN ('waiting', 'active', 'closed')),
  CONSTRAINT rooms_config_snapshot_is_object
    CHECK (jsonb_typeof(config_snapshot) = 'object')
);

CREATE TABLE room_members (
  room_member_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  room_id TEXT NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
  seat TEXT NOT NULL,
  joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  ready BOOLEAN NOT NULL DEFAULT false,
  kicked_at TIMESTAMPTZ,
  left_at TIMESTAMPTZ,
  CONSTRAINT room_members_seat_valid
    CHECK (seat IN ('SEAT_EAST', 'SEAT_SOUTH', 'SEAT_WEST', 'SEAT_NORTH')),
  CONSTRAINT room_members_leave_or_kick_valid
    CHECK (
      (kicked_at IS NULL OR kicked_at >= joined_at)
      AND (left_at IS NULL OR left_at >= joined_at)
    ),
  CONSTRAINT room_members_unique_live_member
    UNIQUE (room_id, user_id)
);

CREATE INDEX idx_user_sessions_user_id_expires_at
  ON user_sessions (user_id, expires_at DESC);

CREATE INDEX idx_rooms_owner_status
  ON rooms (owner_user_id, status);

CREATE INDEX idx_room_members_room_joined_at
  ON room_members (room_id, joined_at ASC);

CREATE INDEX idx_room_members_user_id
  ON room_members (user_id);

CREATE TRIGGER trg_rooms_set_updated_at
BEFORE UPDATE ON rooms
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

COMMENT ON TABLE user_sessions IS
  '游客或正式账户登录后的服务端会话表。WebSocket JoinRoom 使用 session_token 对应的哈希做鉴权。';

COMMENT ON TABLE rooms IS
  '房间大厅表。只表示未开局或已锁定的私密房间，不承载对局事件流。';

COMMENT ON TABLE room_members IS
  '房间成员表。记录大厅阶段的座位、准备态以及离开/踢出轨迹。';

COMMIT;
