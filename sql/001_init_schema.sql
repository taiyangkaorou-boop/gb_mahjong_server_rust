BEGIN;

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.updated_at = now();
  RETURN NEW;
END;
$$;

CREATE TABLE users (
  user_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
  display_name TEXT NOT NULL,
  auth_provider TEXT NOT NULL DEFAULT 'guest',
  auth_subject TEXT,
  avatar_url TEXT,
  profile JSONB NOT NULL DEFAULT '{}'::jsonb,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen_at TIMESTAMPTZ,
  CONSTRAINT users_profile_is_object
    CHECK (jsonb_typeof(profile) = 'object'),
  CONSTRAINT users_auth_subject_unique
    UNIQUE (auth_provider, auth_subject)
);

CREATE TABLE matches (
  match_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
  room_id TEXT NOT NULL,
  status TEXT NOT NULL,
  ruleset_id TEXT NOT NULL,
  config_snapshot JSONB NOT NULL,
  seating_snapshot JSONB NOT NULL,
  current_round_id TEXT,
  prevailing_wind TEXT,
  dealer_seat TEXT,
  started_at TIMESTAMPTZ,
  ended_at TIMESTAMPTZ,
  created_by_user_id TEXT REFERENCES users(user_id),
  winner_user_id TEXT REFERENCES users(user_id),
  last_event_seq BIGINT NOT NULL DEFAULT 0,
  final_result JSONB,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT matches_status_valid
    CHECK (status IN ('waiting', 'active', 'finished', 'aborted')),
  CONSTRAINT matches_prevailing_wind_valid
    CHECK (
      prevailing_wind IS NULL OR prevailing_wind IN (
        'SEAT_EAST',
        'SEAT_SOUTH',
        'SEAT_WEST',
        'SEAT_NORTH'
      )
    ),
  CONSTRAINT matches_dealer_seat_valid
    CHECK (
      dealer_seat IS NULL OR dealer_seat IN (
        'SEAT_EAST',
        'SEAT_SOUTH',
        'SEAT_WEST',
        'SEAT_NORTH'
      )
    ),
  CONSTRAINT matches_config_snapshot_is_object
    CHECK (jsonb_typeof(config_snapshot) = 'object'),
  CONSTRAINT matches_seating_snapshot_is_object
    CHECK (jsonb_typeof(seating_snapshot) = 'object'),
  CONSTRAINT matches_final_result_is_object
    CHECK (final_result IS NULL OR jsonb_typeof(final_result) = 'object'),
  CONSTRAINT matches_last_event_seq_non_negative
    CHECK (last_event_seq >= 0),
  CONSTRAINT matches_time_order_valid
    CHECK (ended_at IS NULL OR started_at IS NULL OR ended_at >= started_at)
);

CREATE TABLE match_events (
  event_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  match_id TEXT NOT NULL REFERENCES matches(match_id) ON DELETE CASCADE,
  round_id TEXT,
  event_seq BIGINT NOT NULL,
  event_type TEXT NOT NULL,
  event_version INTEGER NOT NULL DEFAULT 1,
  actor_user_id TEXT REFERENCES users(user_id),
  actor_seat TEXT,
  request_id TEXT,
  correlation_id TEXT,
  causation_event_seq BIGINT,
  event_payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT match_events_event_seq_positive
    CHECK (event_seq > 0),
  CONSTRAINT match_events_event_type_nonempty
    CHECK (btrim(event_type) <> ''),
  CONSTRAINT match_events_event_version_positive
    CHECK (event_version > 0),
  CONSTRAINT match_events_actor_seat_valid
    CHECK (
      actor_seat IS NULL OR actor_seat IN (
        'SEAT_EAST',
        'SEAT_SOUTH',
        'SEAT_WEST',
        'SEAT_NORTH'
      )
    ),
  CONSTRAINT match_events_causation_event_seq_valid
    CHECK (causation_event_seq IS NULL OR causation_event_seq < event_seq),
  CONSTRAINT match_events_payload_is_object
    CHECK (jsonb_typeof(event_payload) = 'object'),
  CONSTRAINT match_events_unique_stream_position
    UNIQUE (match_id, event_seq)
);

CREATE INDEX idx_users_last_seen_at
  ON users (last_seen_at DESC);

CREATE INDEX idx_matches_room_status
  ON matches (room_id, status);

CREATE INDEX idx_matches_created_by_created_at
  ON matches (created_by_user_id, created_at DESC);

CREATE INDEX idx_matches_started_at
  ON matches (started_at DESC);

CREATE INDEX idx_matches_config_snapshot_gin
  ON matches
  USING GIN (config_snapshot jsonb_path_ops);

CREATE INDEX idx_matches_seating_snapshot_gin
  ON matches
  USING GIN (seating_snapshot jsonb_path_ops);

CREATE INDEX idx_match_events_match_round_seq
  ON match_events (match_id, round_id, event_seq)
  WHERE round_id IS NOT NULL;

CREATE INDEX idx_match_events_actor_created_at
  ON match_events (actor_user_id, created_at DESC)
  WHERE actor_user_id IS NOT NULL;

CREATE INDEX idx_match_events_event_type_created_at
  ON match_events (event_type, created_at DESC);

CREATE INDEX idx_match_events_payload_gin
  ON match_events
  USING GIN (event_payload jsonb_path_ops);

CREATE TRIGGER trg_users_set_updated_at
BEFORE UPDATE ON users
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER trg_matches_set_updated_at
BEFORE UPDATE ON matches
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

COMMENT ON TABLE users IS
  '玩家账户主表。user_id 直接对应 WebSocket/gRPC/事件流里使用的字符串标识。';

COMMENT ON COLUMN users.profile IS $$
扩展资料 JSONB。建议保持扁平对象，便于后续加字段。

示例:
{
  "country": "CN",
  "preferred_locale": "zh-CN",
  "device": {
    "last_platform": "web"
  }
}
$$;

COMMENT ON TABLE matches IS
  '一场完整对局的聚合根快照。保存配置、座位分配、当前游标和最终结算摘要。';

COMMENT ON COLUMN matches.config_snapshot IS $$
镜像 gb_mahjong.client.v1.RoomConfig 的 JSON 快照，作为该局不可变规则配置。

示例:
{
  "ruleset_id": "gb_mahjong_cn_v1",
  "seat_count": 4,
  "enable_flower_tiles": true,
  "enable_robbing_kong": true,
  "enable_kong_draw": true,
  "reconnect_grace_seconds": 30,
  "action_timeout_ms": 15000
}
$$;

COMMENT ON COLUMN matches.seating_snapshot IS $$
固定四个位次的用户分配，推荐按 Seat 枚举名做 key，便于直接定位东南西北。

示例:
{
  "SEAT_EAST": {
    "user_id": "usr_east",
    "display_name": "Alice",
    "initial_score": 25000
  },
  "SEAT_SOUTH": {
    "user_id": "usr_south",
    "display_name": "Bob",
    "initial_score": 25000
  },
  "SEAT_WEST": {
    "user_id": "usr_west",
    "display_name": "Carol",
    "initial_score": 25000
  },
  "SEAT_NORTH": {
    "user_id": "usr_north",
    "display_name": "Dave",
    "initial_score": 25000
  }
}
$$;

COMMENT ON COLUMN matches.final_result IS $$
整场结束后的摘要，可直接镜像 gb_mahjong.client.v1.MatchSettlement 的业务字段。

示例:
{
  "finished_at_unix_ms": "1760000000000",
  "standings": [
    {
      "seat": "SEAT_EAST",
      "user_id": "usr_east",
      "display_name": "Alice",
      "final_score": 38200,
      "rank": 1,
      "rounds_won": 3,
      "self_draw_wins": 1
    }
  ]
}
$$;

COMMENT ON TABLE match_events IS
  '事件溯源主表。每一行是一条已提交的服务端权威事件，按 match_id + event_seq 严格排序。';

COMMENT ON COLUMN match_events.event_type IS $$
建议直接使用 ServerFrame oneof 的 case 名，例如:
- join_room
- sync_state
- action_broadcast
- action_prompt
- action_rejected
- player_connection_changed
- round_settlement
- match_settlement
- pong
$$;

COMMENT ON COLUMN match_events.event_payload IS $$
存储具体事件消息体的 JSONB。match_id、event_seq、event_type 已经被拆成首类索引列；
如果某个 protobuf 消息体本身也包含这些字段，则按消息定义原样保留，不做裁剪。
Rust 侧可将 protobuf 消息体通过 serde/pbjson 序列化后写入；回放时再根据 event_type
反序列化为具体 payload，并结合 event_seq 包装回 ServerFrame。

下面示例使用 snake_case 字段名表达结构；如果实现阶段采用 protobuf canonical JSON
的 lowerCamelCase 输出，也不影响本表设计，因为查询主要依赖首类列和 JSONB 路径索引。

示例 1: action_broadcast
{
  "room_id": "room_001",
  "match_id": "match_001",
  "round_id": "east_1",
  "event_seq": "42",
  "actor_seat": "SEAT_EAST",
  "action_kind": "ACTION_KIND_DISCARD",
  "action_detail": {
    "discard": {
      "tile": "TILE_CHARACTER_5",
      "tsumogiri": false
    }
  },
  "resulting_effects": [
    {
      "discard_added": {
        "seat": "SEAT_EAST",
        "discard": {
          "tile": "TILE_CHARACTER_5",
          "turn_index": 7,
          "claimed": false,
          "drawn_and_discarded": false,
          "source_event_seq": "42"
        }
      }
    }
  ],
  "resulting_phase": "GAME_PHASE_WAITING_CLAIM",
  "next_turn_seat": "SEAT_SOUTH",
  "wall_tiles_remaining": 67,
  "action_deadline_unix_ms": "1760000015000"
}

示例 2: round_settlement
{
  "room_id": "room_001",
  "match_id": "match_001",
  "round_id": "east_1",
  "prevailing_wind": "SEAT_EAST",
  "hand_number": 1,
  "dealer_seat": "SEAT_EAST",
  "win_type": "WIN_TYPE_SELF_DRAW",
  "winner_seat": "SEAT_SOUTH",
  "discarder_seat": "SEAT_UNSPECIFIED",
  "winning_tile": "TILE_DOT_3",
  "player_results": [
    {
      "seat": "SEAT_SOUTH",
      "round_delta": 24,
      "total_score_after": 25024
    }
  ],
  "fan_details": [
    {
      "fan_code": "SELF_DRAW",
      "fan_name": "自摸",
      "fan_value": 1,
      "count": 1,
      "description": "赢家自摸和牌"
    }
  ],
  "settlement_flags": [
    "SETTLEMENT_FLAG_SELF_DRAW"
  ],
  "wall_tiles_remaining": 66
}
$$;

COMMIT;
