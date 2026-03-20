#pragma once

#include <optional>
#include <string>
#include <vector>

#include "internal_engine.pb.h"

namespace gb_mahjong::engine_server {

// 适配层负责把项目内部 protobuf 协议转换成 GB-Mahjong 能理解的数据结构。
// 房间并发、抢操作优先级、窗口超时等问题都不在这里处理。
class GbMahjongAdapter {
 public:
  struct ValidateOutcome {
    // ValidateOutcome 描述的是"候选动作是否合法，以及如果合法会导出什么规则结果"。
    bool legal = false;
    gb_mahjong::engine::v1::ValidationRejectCode reject_code =
        gb_mahjong::engine::v1::VALIDATION_REJECT_CODE_UNSPECIFIED;
    gb_mahjong::engine::v1::ActionKind derived_action_type =
        gb_mahjong::engine::v1::ACTION_KIND_UNSPECIFIED;
    std::string explanation;
    std::optional<gb_mahjong::engine::v1::MeldResult> meld_result;
    std::optional<gb_mahjong::engine::v1::WinningContext> winning_context;
    std::vector<gb_mahjong::engine::v1::SuggestedFollowUp> follow_ups;
  };

  struct ScoreOutcome {
    // ScoreOutcome 是算番/结算层的纯计算结果，不包含房间状态推进副作用。
    uint32_t total_fan = 0;
    std::vector<gb_mahjong::engine::v1::FanDetail> fan_details;
    std::vector<gb_mahjong::engine::v1::SeatScoreDelta> score_delta_by_seat;
    std::vector<gb_mahjong::engine::v1::SettlementFlag> settlement_flags;
  };

  // ValidateAction 只做无状态规则校验。
  ValidateOutcome ValidateAction(
      const gb_mahjong::engine::v1::ValidateActionRequest& request) const;

  // CalculateScore 只读取终局上下文并返回番型与分数结果。
  ScoreOutcome CalculateScore(
      const gb_mahjong::engine::v1::CalculateScoreRequest& request) const;

 private:
  static gb_mahjong::engine::v1::ActionKind DeduceActionKind(
      const gb_mahjong::engine::v1::CandidateAction& candidate_action);

  static gb_mahjong::engine::v1::MeldKind DeduceMeldKind(
      const gb_mahjong::engine::v1::ClaimAction& claim_action);
};

}  // namespace gb_mahjong::engine_server
