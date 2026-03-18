#include <cstdlib>
#include <iostream>
#include <memory>
#include <string>

#include <grpcpp/grpcpp.h>

#include "gb_mahjong_adapter.h"
#include "rule_engine_service.h"

namespace gb_mahjong::engine_server {

std::string ReadEnvOrDefault(const char* key, const char* fallback) {
  const char* value = std::getenv(key);
  if (value == nullptr || std::string(value).empty()) {
    return fallback;
  }

  return value;
}

}  // namespace gb_mahjong::engine_server

int main() {
  using gb_mahjong::engine_server::GbMahjongAdapter;
  using gb_mahjong::engine_server::ReadEnvOrDefault;
  using gb_mahjong::engine_server::RuleEngineServiceImpl;

  const std::string host =
      ReadEnvOrDefault("GB_MAHJONG_ENGINE_HOST", "0.0.0.0");
  const std::string port =
      ReadEnvOrDefault("GB_MAHJONG_ENGINE_PORT", "50051");
  const std::string listen_addr = host + ":" + port;

  auto adapter = std::make_shared<GbMahjongAdapter>();
  RuleEngineServiceImpl service(adapter);

  grpc::ServerBuilder builder;
  builder.AddListeningPort(listen_addr, grpc::InsecureServerCredentials());
  builder.RegisterService(&service);
  builder.SetSyncServerOption(
      grpc::ServerBuilder::SyncServerOption::NUM_CQS, 2);
  builder.SetSyncServerOption(
      grpc::ServerBuilder::SyncServerOption::MIN_POLLERS, 1);
  builder.SetSyncServerOption(
      grpc::ServerBuilder::SyncServerOption::MAX_POLLERS, 4);

  std::unique_ptr<grpc::Server> server(builder.BuildAndStart());
  if (server == nullptr) {
    std::cerr << "failed to start rule engine gRPC server on " << listen_addr
              << std::endl;
    return 1;
  }

  std::cout << "gb mahjong rule engine listening on " << listen_addr
            << std::endl;
  server->Wait();
  return 0;
}
