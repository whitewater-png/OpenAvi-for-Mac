#!/bin/bash
# OpenAvi macOS パッケージ作成スクリプト
#
#   ./package-macos.sh              # リリースビルド → dist/OpenAvi.app と dist/OpenAvi-macos-<arch>.zip を生成
#   ./package-macos.sh --skip-build # 既存の target/release/openavi を使ってパッケージのみ作成
#
# 生成した OpenAvi.app を「アプリケーション」フォルダへドラッグすればインストール完了です。

set -euo pipefail
cd "$(dirname "$0")"

SKIP_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=1 ;;
        *) echo "不明なオプション: $arg" >&2; exit 1 ;;
    esac
done

export PATH="$HOME/.cargo/bin:$PATH"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
if [ -z "$VERSION" ]; then
    echo "Cargo.toml からバージョンを取得できません" >&2
    exit 1
fi

# 1. ビルド
if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "OpenAvi をビルドしています..."
    cargo build --release -p au2-app
fi
BIN="${CARGO_TARGET_DIR:-target}/release/openavi"
if [ ! -x "$BIN" ]; then
    echo "実行ファイルが見つかりません: $BIN" >&2
    exit 1
fi
# Rosetta 上のシェルでも正しく判定できるよう、シェルではなくバイナリからアーキテクチャを取得
ARCH="$(lipo -archs "$BIN" | tr ' ' '-')"

# 2. .app バンドルの骨組みを作成
APP="dist/OpenAvi.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/openavi"
cp -R samples plugins "$APP/Contents/Resources/"
cp README.md LICENSE "$APP/Contents/Resources/"

# 3. assets/openavi.ico (256x256 PNG 内蔵) から .icns を生成
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ICONSET="$WORK/openavi.iconset"
mkdir -p "$ICONSET"
sips -s format png assets/openavi.ico --out "$WORK/icon_256.png" >/dev/null
for size in 16 32 128 256; do
    sips -z "$size" "$size" "$WORK/icon_256.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
done
for size in 16 32 128; do
    double=$((size * 2))
    sips -z "$double" "$double" "$WORK/icon_256.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/openavi.icns"

# 4. Info.plist
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>OpenAvi</string>
    <key>CFBundleDisplayName</key>
    <string>OpenAvi</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.openavi</string>
    <key>CFBundleExecutable</key>
    <string>openavi</string>
    <key>CFBundleIconFile</key>
    <string>openavi</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleDevelopmentRegion</key>
    <string>ja</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# 5. アドホック署名（Apple Silicon で起動するために必要。配布用の Developer ID 署名ではありません）
#    Finder 情報などの拡張属性が残っていると codesign が失敗するため事前に除去
xattr -cr "$APP"
codesign --force --deep --sign - "$APP"
echo "Created app bundle: $APP"

# 6. 配布用 ZIP
ZIP="dist/OpenAvi-macos-${ARCH}.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
echo "Created release archive: $ZIP"
