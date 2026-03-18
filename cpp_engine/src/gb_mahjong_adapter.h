#pragma once

#include <optional>
#include <string>
#include <vector>

#include "internal_engine.pb.h"

namespace gb_mahjong::engine_server {

class GbMahjongAdapter {
 public:
  struct ValidateOutcome {
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
    uint32_t total_fan = 0;
    std::vector<gb_mahjong::engine::v1::FanDetail> fan_details;
    std::vector<gb_mahjong::engine::v1::SeatScoreDelta> score_delta_by_seat;
    std::vector<gb_mahjong::engine::v1::SettlementFlag> settlement_flags;
  };

  ValidateOutcome ValidateAction(
      const gb_mahjong::engine::v1::ValidateActionRequest& request) const;

  ScoreOutcome CalculateScore(
      const gb_mahjong::engine::v1::CalculateScoreRequest& request) const;

 private:
  static gb_mahjong::engine::v1::ActionKind DeduceActionKind(
      const gb_mahjong::engine::v1::CandidateAction& candidate_action);

  static gb_mahjong::engine::v1::MeldKind DeduceMeldKind(
      const gb_mahjong::engine::v1::ClaimAction& claim_action);
};

}  // namespace gb_mahjong::engine_server

