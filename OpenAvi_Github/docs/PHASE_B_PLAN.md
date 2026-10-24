# 段階 B 作業分担（並列開発用）

段階 B = 拡張編集の描画仕様（ROADMAP §4）。5 つの独立した担当に分け、それぞれ別の worktree / ブランチで進めて統合する。
**他の担当の領域を書き換えない**（衝突を避けるため）。共有ファイルを触る場合は、自分の担当部分だけを最小限に。

| 担当 | ブランチ名 | 主なファイル（所有） | 内容 |
|---|---|---|---|
| U1 動き | `phase-b/motion` | `au2-core/src/track.rs`, `au2-exo/src/{reader,writer}.rs` | 移動量指定・ランダム移動・反復移動・加減速などの移動方法の評価と `.exo` の読み書き |
| U2 テキスト | `phase-b/text` | `au2-render/src/text.rs`, `renderer.rs` の `EffectKind::Text` の分岐だけ | 文字の影・縁取り・文字間隔・太字/斜体・揃え・フォント選択・表示速度 |
| U3 合成 | `phase-b/blend` | `shaders/sprite_fragment.wgsl`, `renderer.rs` の描画パス記録、`SpriteUniform::blend` | 合成モード 12 種、上のオブジェクトでクリッピング |
| U4 3D | `phase-b/draw3d` | `shaders/sprite_vertex.wgsl`, `eval.rs`, `SpriteUniform::ext` | 拡張描画（XYZ 回転・中心・裏面）、カメラ制御（3D 射影）、グループ制御 |
| U5 時間・シーン | `phase-b/scene` | `renderer.rs` のシーン/時間制御、`media.rs` | 時間制御、シーン（別シーンの描画をテクスチャ化）、直前オブジェクト、フレームバッファ |

## 共通の約束

- 土台: `SpriteUniform` に予約領域 `ext: [f32;16]`（U4）と `blend: [u32;4]`（U3）、WGSL の `Draw` にも同じ並びで `ext` / `blend`。
  シェーダは `sprite_common.wgsl` + `sprite_vertex.wgsl` + `sprite_fragment.wgsl` を連結して使う。
- 実 AviUtl の挙動が分からない箇所は「近似」と明記し、`docs/RENDER_DESIGN.md` の未決事項に書く（段階 E で参照画像と突き合わせる）。
- 各機能に、**修正（実装）を外すと失敗するテスト**を付ける（実装を無効化して失敗することを確認する）。
- GPU テストは `Gpu::headless()` が使えないときスキップ。ゴールデン画像を足す場合は `UPDATE_GOLDEN=1` で生成し、目視確認する。
- `cargo test --workspace` と `MTL_DEBUG_LAYER=1 cargo test -p au2-render` が通ること。clippy で新しい警告を増やさない。
- コミットメッセージは英語、末尾に `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。
