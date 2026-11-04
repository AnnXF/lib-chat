# 本地开发入口。
#
# 服务端固定在 127.0.0.1:5566，自签身份放在 dev/ 下；
# 客户端在构建时读这份身份，把地址与指纹编译进去，登录页只让人填账号密码。

ADDR ?= 127.0.0.1:5566
ROOM ?= 本地聊天室
DATA ?= dev
PID  := .dev-server.pid

.PHONY: help dev serve ui identity stop test fmt lint clean

help:
	@echo "make dev       起服务端（$(ADDR)）并打开界面，退出界面时关掉服务端"
	@echo "make serve     只起服务端"
	@echo "make ui        只开界面（需要服务端已在运行）"
	@echo "make identity  生成/读取 dev 下的自签身份并打印指纹"
	@echo "make test      跑完整校验（格式、Clippy、全量测试、UI 布局、压测）"
	@echo "make stop      关掉后台服务端"
	@echo "make clean     清掉构建产物与本地数据"

# 身份要先于界面构建存在，build.rs 才能把指纹编进去
dev: identity
	@echo "==> 服务端 $(ADDR)（数据在 $(DATA)/）"
	@./target/debug/libchat serve --listen $(ADDR) --name "$(ROOM)" --data-dir $(DATA) & echo $$! > $(PID); \
	cargo run -p chat-ui-gpui; \
	kill $$(cat $(PID) 2>/dev/null) 2>/dev/null || true; rm -f $(PID)

serve: identity
	@cargo run -p chat-cli -- serve --listen $(ADDR) --name "$(ROOM)" --data-dir $(DATA)

ui:
	@cargo run -p chat-ui-gpui

identity:
	@cargo build -q -p chat-cli
	@./target/debug/libchat identity --dir $(DATA)

stop:
	@kill $$(cat $(PID) 2>/dev/null) 2>/dev/null || true; rm -f $(PID)

test:
	@./scripts/verify.sh

fmt:
	@cargo fmt --all

lint:
	@cargo clippy --workspace --all-targets -- -D warnings

clean:
	@cargo clean
	@rm -rf $(DATA) $(PID)
