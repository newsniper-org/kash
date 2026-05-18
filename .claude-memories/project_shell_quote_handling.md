---
name: kash — quote handling (committed)
description: '/`/\$'... 표준, $'...' ANSI-C 모든 모드 채택 (bash + ksh93 form 둘 다), $"..."는 kash 미지원 (transpiler gettext plugin으로)
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash quoting 시스템 확정 사항. (관련: project_shell_transpiler.md)

## POSIX core (모든 모드)

- `'literal'` — expansion 없음
- `"$var and \$escape"` — param/cmd/arith subst, `\` `"` `$` `` ` `` escapable
- `\X` — single-char escape outside quotes
- `\` line continuation

## `$'...'` ANSI-C — 모든 모드 채택 (POSIX-strict 포함)

POSIX.1-2024 정식 채택 → POSIX-strict에서도 가용.

### Escape 지원 (bash superset)
- `\n`, `\t`, `\r`, `\b`, `\a`, `\f`, `\v`, `\e`/`\E`, `\\`, `\'`, `\"`, `\?`
- `\xHH` — hex byte (1-2 digits)
- `\nnn` — octal (1-3 digits)
- `\uHHHH` — Unicode (4 hex, bash form)
- `\UHHHHHHHH` — Unicode (8 hex, bash form)
- `\u{HEX}` — Unicode (variable hex, **ksh93 form 동시 채택**)

bash와 ksh93의 Unicode form 모두 받음 — `{` 유무로 parsing 모호성 없음.

## `$"..."` gettext localization — **kash 자체 미지원**

bash/ksh93의 `$"..."`는 kash native syntax가 아님. 대신 **script transpiler의 gettext plugin**이 처리:
- bash `$"hello"` → kash `$(gettext "hello")` 같은 직접 호출로 변환
- Plugin은 kash script 파일 형태 (별도 구현)

이는 "bash 호환은 transpiler가 담당" 일관 정책 적용.

## 그 외

- `echo` escape: `print`/`printf` 권장 ([18-builtins.md](18-builtins.md))

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `'...'`, `"..."`, `\X` | ✓ | ✓ | ✓ | ✓ | ✓ |
| `$'...'` ANSI-C (bash+ksh93 superset) | ✓ (POSIX-2024) | ✓ | ✓ | ✓ | ✓ |
| `\u{HEX}` (ksh93 form) | (POSIX 미정의) | ✓ | ✓ | ✓ | ✓ |
| `\` line continuation | ✓ | ✓ | ✓ | ✓ | ✓ |
| `$"..."` native | × | × | × | × | × (transpiler plugin) |

## 미결

남은 v2+ 항목:
- Locale-specific quoting (RTL 등) — v2 검토

기타 모두 project_kash_sweep_v1.md에서 해소 — `\u{HEX}` 자릿수 1-6 (max U+10FFFF), POSIX-strict 미정의 escape는 error.

**How to apply:** quote 관련 후속 결정 (raw string literal, multi-line string 등) 시 위 베이스. `$"..."` 같은 bash-only feature는 항상 transpiler plugin 경로.
