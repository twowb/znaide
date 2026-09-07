# znaide 跨平台构建
# 用法:
#   make build            编译本机平台(自动检测)
#   make build-linux      编译 Linux x86_64(musl 全静态)
#   make build-linux-arm64 编译 Linux aarch64(musl 全静态)
#   make build-windows    编译 Windows x86_64(GNU + CRT 静态)
#   make build-macos      编译 macOS x86_64(zig 交叉编译)
#   make build-macos-arm64 编译 macOS aarch64(zig 交叉编译)
#   make build-android    编译 Android aarch64(NDK bionic 动态,Termux 用)
#   make build-all        全部平台(缺工具链自动跳过)
#   make dist             查看 dist/ 产物
#   make clean            清理 target/ 与 dist/
#
# 所有构建均为 --release(体积最小化配置见 Cargo.toml [profile.release]),
# Linux/Windows 为全静态,产物在 dist/。

APP      := znaide
PROFILE  := release
DIST     := dist
TARGET   := target
# 版本号:自动取根 Cargo.toml 的 [workspace.package] version——bump 只改那一行
VERSION  := $(shell grep -m1 'version = "' Cargo.toml | sed 's/.*"\([^"]*\)".*/\1/')

# 本机检测(uname)
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

# ---- 平台 triples ----
TRIPLE_LINUX_X64    := x86_64-unknown-linux-musl
TRIPLE_LINUX_ARM64  := aarch64-unknown-linux-musl
TRIPLE_WIN_X64      := x86_64-pc-windows-gnu
TRIPLE_MAC_X64      := x86_64-apple-darwin
TRIPLE_MAC_ARM64    := aarch64-apple-darwin
TRIPLE_ANDROID      := aarch64-linux-android

# Android NDK(bionic 动态版,Termux 用)。NDK_ROOT 可用环境变量/命令行覆盖。
NDK_ROOT ?= $(or $(ANDROID_NDK_HOME),/home/zngeek/znbin/android-ndk-r27)
NDK_API  ?= 26
NDK_BIN  := $(NDK_ROOT)/toolchains/llvm/prebuilt/linux-x86_64/bin
CC_ANDROID := $(NDK_BIN)/aarch64-linux-android$(NDK_API)-clang
AR_ANDROID := $(NDK_BIN)/llvm-ar

# 判断本机 triple
ifeq ($(UNAME_S),Linux)
  ifeq ($(UNAME_M),x86_64)
    NATIVE_TRIPLE := $(TRIPLE_LINUX_X64)
  else ifeq ($(UNAME_M),aarch64)
    NATIVE_TRIPLE := $(TRIPLE_LINUX_ARM64)
  else
    $(error 暂不支持的 Linux 架构: $(UNAME_M))
  endif
else ifeq ($(UNAME_S),Darwin)
  ifeq ($(UNAME_M),arm64)
    NATIVE_TRIPLE := $(TRIPLE_MAC_ARM64)
  else
    NATIVE_TRIPLE := $(TRIPLE_MAC_X64)
  endif
else
  NATIVE_TRIPLE := $(TRIPLE_WIN_X64)
endif

# ---- 工具链检查 ----
CC_LINUX_X64    := x86_64-linux-musl-gcc
CC_LINUX_ARM64  := aarch64-linux-musl-gcc
CC_WIN_X64      := x86_64-w64-mingw32-gcc
# macOS 用 zig 交叉编译(无需 osxcross/SDK):linker 与 CC 指向 wrapper
# wrapper(zig-cc-x86_64-darwin / zig-cc-aarch64-darwin)置于 ~/.cargo/bin,
# 内容:exec zig cc -target <triple>-macos-none "$@",并丢弃 cc crate 附加的 --target;
# 对应 .cargo/config.toml 的 darwin 配置 + ~/.cargo/darwin-stubs/libiconv.tbd
CC_MAC_X64      := zig-cc-x86_64-darwin
CC_MAC_ARM64    := zig-cc-aarch64-darwin

.PHONY: all build build-linux build-linux-arm64 build-windows build-macos build-macos-arm64 build-android build-all dist clean help

default: build

help:
	@echo "znaide 跨平台构建"
	@echo "  make build               编译本机平台"
	@echo "  make build-linux          Linux x86_64 (musl 静态)"
	@echo "  make build-linux-arm64    Linux aarch64 (musl 静态)"
	@echo "  make build-windows        Windows x86_64 (CRT 静态)"
	@echo "  make build-macos          macOS x86_64 (zig 交叉编译)"
	@echo "  make build-macos-arm64    macOS aarch64 (zig 交叉编译)"
	@echo "  make build-android        Android aarch64 (NDK bionic 动态;NDK_ROOT 可覆盖)"
	@echo "  make build-all            全部平台(缺工具链自动跳过)"
	@echo "  make dist                查看产物"
	@echo "  make clean               清理构建产物"

# 检查交叉编译器是否存在,缺失时给出提示
define check_cc
	@if ! command -v $(1) >/dev/null 2>&1; then \
		echo "错误: 缺少交叉编译器 $(1)"; \
		echo "  Linux x86_64: sudo apt install musl-tools"; \
		echo "  Linux arm64:  安装 musl 交叉工具链(如 https://more.musl.cc/ 的 aarch64-linux-musl-cross)并加入 PATH"; \
		echo "  Windows:      sudo apt install gcc-mingw-w64-x86-64"; \
		exit 1; \
	fi
endef

# macOS 走 zig:检查 zig 与 darwin wrapper 是否可用
define check_mac_toolchain
	@if ! command -v zig >/dev/null 2>&1; then \
		echo "错误: 缺少 zig(macOS 交叉编译必需)"; \
		echo "  下载: https://ziglang.org/download/ 并加入 PATH"; \
		exit 1; \
	fi; \
	if ! command -v $(1) >/dev/null 2>&1; then \
		echo "错误: 缺少交叉编译器 $(1)"; \
		echo "  需要 ~/.cargo/bin/zig-cc-{x86_64,aarch64}-darwin wrapper:"; \
		echo "    exec zig cc -target <triple>-macos-none \"\$$@\"(并丢弃 clang 风格 --target)"; \
		exit 1; \
	fi
endef

# Android 走 NDK:检查 clang/ar 是否存在(路径由 NDK_ROOT 决定)
define check_ndk
	@if [ ! -x "$(CC_ANDROID)" ] || [ ! -x "$(AR_ANDROID)" ]; then \
		echo "错误: 缺少 Android NDK 交叉编译器: $(CC_ANDROID)"; \
		echo "  请安装 NDK(如 android-ndk-r27)或用 NDK_ROOT=<路径> 覆盖:"; \
		echo "    make build-android NDK_ROOT=/path/to/android-ndk-rXX"; \
		echo "  或设置 ANDROID_NDK_HOME 环境变量"; \
		exit 1; \
	fi
endef

# 检查 rustup target 是否已装,未装自动补
define check_target
	@if ! rustup target list --installed | grep -qx '$(1)'; then \
		echo "==> rustup target add $(1)"; \
		rustup target add $(1) || exit 1; \
	fi
endef

# 构建单个 triple:检查工具链 → cargo build(导出 CC_<triple> 供 ring 等 C 依赖)→ 拷贝到 dist
# 产物名带版本(如 znaide-linux-x64-v1.0.0),与 GitHub Releases 资产对齐
define build_one
	$(call check_cc,$(2))
	$(call check_target,$(1))
	@echo "==> cargo build --release --target $(1)"
	@export CC_$(subst -,_,$(1))="$(2)"; cargo build --release --target $(1)
	@mkdir -p $(DIST)
	@cp $(TARGET)/$(1)/$(PROFILE)/$(APP)$(3) $(DIST)/$(APP)-$(4)-v$(VERSION)$(3)
	@echo "==> 产物: $(DIST)/$(APP)-$(4)-v$(VERSION)$(3)"
	@ls -lh $(DIST)/$(APP)-$(4)-v$(VERSION)$(3)
endef

build: ## 编译本机平台
	@echo "本机: $(NATIVE_TRIPLE)"
ifeq ($(NATIVE_TRIPLE),$(TRIPLE_LINUX_X64))
	$(call build_one,$(TRIPLE_LINUX_X64),$(CC_LINUX_X64),,linux-x64)
else ifeq ($(NATIVE_TRIPLE),$(TRIPLE_LINUX_ARM64))
	$(call build_one,$(TRIPLE_LINUX_ARM64),$(CC_LINUX_ARM64),,linux-arm64)
else ifeq ($(NATIVE_TRIPLE),$(TRIPLE_WIN_X64))
	$(call build_one,$(TRIPLE_WIN_X64),$(CC_WIN_X64),.exe,windows-x64)
else ifeq ($(NATIVE_TRIPLE),$(TRIPLE_MAC_X64))
	$(call build_one,$(TRIPLE_MAC_X64),$(CC_MAC_X64),,macos-x64)
else ifeq ($(NATIVE_TRIPLE),$(TRIPLE_MAC_ARM64))
	$(call build_one,$(TRIPLE_MAC_ARM64),$(CC_MAC_ARM64),,macos-arm64)
else
	$(error 无法识别本机平台)
endif

build-linux: ## Linux x86_64 (musl 全静态)
	$(call build_one,$(TRIPLE_LINUX_X64),$(CC_LINUX_X64),,linux-x64)

build-linux-arm64: ## Linux aarch64 (musl 全静态)
	$(call build_one,$(TRIPLE_LINUX_ARM64),$(CC_LINUX_ARM64),,linux-arm64)

build-windows: ## Windows x86_64 (GNU + CRT 静态)
	$(call build_one,$(TRIPLE_WIN_X64),$(CC_WIN_X64),.exe,windows-x64)

build-macos: ## macOS x86_64 (zig 交叉编译)
	$(call check_mac_toolchain,$(CC_MAC_X64))
	$(call build_one,$(TRIPLE_MAC_X64),$(CC_MAC_X64),,macos-x64)

build-macos-arm64: ## macOS aarch64 (zig 交叉编译)
	$(call check_mac_toolchain,$(CC_MAC_ARM64))
	$(call build_one,$(TRIPLE_MAC_ARM64),$(CC_MAC_ARM64),,macos-arm64)

# Android:bionic 动态版(Termux 用,可正常解析 DNS)。
# 需要 NDK(rust 无 C 依赖无法用 zig 交叉:zig 不内置 bionic libc)。
# 产物: dist/znaide-android-arm64,拷到手机 Termux 直接运行。
build-android: ## Android aarch64 (NDK bionic 动态,Termux 用)
	$(call check_ndk)
	$(call check_target,$(TRIPLE_ANDROID))
	@echo "==> cargo build --release --target $(TRIPLE_ANDROID)"
	@export CC_aarch64_linux_android="$(CC_ANDROID)" \
	        AR_aarch64_linux_android="$(AR_ANDROID)" \
	        CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$(CC_ANDROID)"; \
	  cargo build --release --target $(TRIPLE_ANDROID)
	@mkdir -p $(DIST)
	@cp $(TARGET)/$(TRIPLE_ANDROID)/$(PROFILE)/$(APP) $(DIST)/$(APP)-android-arm64-v$(VERSION)
	@echo "==> 产物: $(DIST)/$(APP)-android-arm64-v$(VERSION) (Android bionic 动态版,Termux 用)"
	@ls -lh $(DIST)/$(APP)-android-arm64-v$(VERSION)

# 全部平台:工具链缺失的自动跳过(musl 两档 + mingw + zig 均需就绪)
build-all:
	@echo "==> build-all(缺工具链的平台自动跳过)"
	@$(MAKE) --no-print-directory build-linux
	@$(MAKE) --no-print-directory build-linux-arm64
	@if command -v $(CC_WIN_X64) >/dev/null 2>&1; then $(MAKE) --no-print-directory build-windows; else echo "!! 跳过 windows-x64:缺 $(CC_WIN_X64)"; fi
	@if command -v zig >/dev/null 2>&1 && command -v $(CC_MAC_X64) >/dev/null 2>&1; then $(MAKE) --no-print-directory build-macos; else echo "!! 跳过 macos-x64:缺 zig 或 $(CC_MAC_X64)"; fi
	@if command -v zig >/dev/null 2>&1 && command -v $(CC_MAC_ARM64) >/dev/null 2>&1; then $(MAKE) --no-print-directory build-macos-arm64; else echo "!! 跳过 macos-arm64:缺 zig 或 $(CC_MAC_ARM64)"; fi
	@if [ -x "$(CC_ANDROID)" ] && [ -x "$(AR_ANDROID)" ]; then $(MAKE) --no-print-directory build-android; else echo "!! 跳过 android-arm64:缺 NDK($(CC_ANDROID))"; fi
	@echo "==> build-all 完成,产物见 $(DIST)/"

dist:
	@ls -lh $(DIST)/ 2>/dev/null || echo "dist/ 为空,先 make build"

clean:
	@cargo clean
	@rm -rf $(DIST)
	@echo "已清理 target/ 与 $(DIST)/"
