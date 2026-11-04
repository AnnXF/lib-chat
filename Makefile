# 本地开发入口。
#
# 服务端固定在 127.0.0.1:5566，自签身份放在 dev/ 下；
# 客户端在构建时读这份身份，把地址与指纹编译进去，登录页只让人填账号密码。
#
# 起多个客户端：终端 1 跑 `make serve`，终端 2、3、4 各跑一次 `make ui`，
# 每个客户端用不同账号登录（互发消息就是多端测试）。

ADDR ?= 127.0.0.1:5566
ROOM ?= 本地聊天室
DATA ?= dev
BIN  := ./target/debug/libchat

# 客户端构建脚本读这个变量决定内置地址；两边用同一个值，避免对不上
export LIBCHAT_DEV_ADDR := $(ADDR)
PID  := .dev-server.pid
LOG  := $(DATA)/server.log

.PHONY: help serve ui dev stop identity build test fmt lint clean

help:
	@echo "终端 1      make serve   前台跑服务端（$(ADDR)）"
	@echo "终端 2、3…  make ui      每个终端开一个客户端，用不同账号登录"
	@echo "或者        make dev     后台起服务端并开一个客户端；服务端留着，之后可继续 make ui"
	@echo "            make stop    关掉后台服务端"
	@echo "            make identity  生成/读取 dev 下的自签身份并打印指纹"
	@echo "            make test    完整校验"
	@echo "            make clean   清掉构建产物与本地数据"

# 前台跑服务端：占住一个终端，客户端在别的终端开
serve: identity
	@$(BIN) serve --listen $(ADDR) --name "$(ROOM)" --data-dir $(DATA)

# 一个客户端。界面上地址与指纹已经内置，只填账号密码
ui:
	@cargo run -p chat-ui-gpui

# 省事模式：服务端丢到后台（nohup，关掉界面也不退出），再开一个客户端
dev: identity
	@if [ -f $(PID) ] && kill -0 $$(cat $(PID)) 2>/dev/null; then \
		echo "==> 服务端已在运行（pid $$(cat $(PID))）"; \
	else \
		echo "==> 后台启动服务端 $(ADDR)（日志 $(LOG)）"; \
		nohup $(BIN) serve --listen $(ADDR) --name "$(ROOM)" --data-dir $(DATA) > $(LOG) 2>&1 & \
		echo $$! > $(PID); sleep 1; \
	fi
	@echo "==> 打开客户端；再开一个用 make ui，关后台服务端用 make stop"
	@cargo run -p chat-ui-gpui

stop:
	@if [ -f $(PID) ]; then kill $$(cat $(PID) 2>/dev/null) 2>/dev/null || true; rm -f $(PID); echo "已停止后台服务端"; else echo "没有后台服务端"; fi

build:
	@cargo build -q -p chat-cli

# 身份要先于界面构建存在，build.rs 才能把指纹编进去
identity: build
	@$(BIN) identity --dir $(DATA)

test:
	@./scripts/verify.sh

fmt:
	@cargo fmt --all

lint:
	@cargo clippy --workspace --all-targets -- -D warnings

clean:
	@$(MAKE) --no-print-directory stop
	@cargo clean
	@rm -rf $(DATA)
