---
name: New shell — zsh-style parameter expansion flags (committed)
description: ${(flags)var} 표기의 채택 범위, 평가 순서, compound 확장, `-secure` mode와의 상호작용 확정
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 parameter expansion flag 시스템 확정 사항. (관련: project_shell_compound_vars.md, project_shell_modes.md)

## 채택 범위 — zsh 전체 + compound 확장

- zsh의 *모든* expansion flag 채택. Curated subset 가는 순간 사용자가 "이 flag는 있나" 매번 확인해야 하므로 학습 부담 오히려 증가.
- 카테고리: 분할/결합 `(s/j/f/F/z)`, 정렬/중복 `(o/O/n/i/u)`, 인용 `(q/qq/qqq/qqqq/Q)`, 대소문자 `(L/U/C)`, 간접/메타 `(P/t/@/e/D)`, assoc `(k/v/kv)`, 경로 modifier `(:h/:t/:r/:e)`, 기타 `(g/V/c/#/%)`.
- **Compound 확장**:
  - `${(k)compound}` → 멤버 이름 리스트 (assoc 키와 의미 통일)
  - `${(v)compound}` → 멤버 값 리스트
  - `${(kv)compound}` → key/value interleaved
  - `${(t)compound.member}` → 그 멤버의 type 문자열 ("scalar", "compound", "indexed", "assoc")
- ksh93 `${!person.@}` 표기는 ksh93u-strict 모드에서만 보존, default 모드에서는 `${(k)person}`로 통일.

## 평가 순서 — zsh order 그대로

Flag 적용 순서는 *flag 위치 무관* 하게 고정:

```
(P) indirect
  → subscripting/expansion
  → (e) re-eval
  → modifiers (:h, :t, :r, :e)
  → (s) split
  → (o)/(O)/(n)/(i)/(u) sort/dedupe
  → (@) array preserve
  → (j) join
  → (q)/(Q) quote/unquote
  → (L)/(U)/(C) case
```

`${(@s.,.)var}` ≡ `${(s.,.@)var}` — 결과가 flag *조합*에만 의존, 위치 무관.

## Flag 인자 구분자 — Perl-style paired delim

- `(s:,:)` = `:`, `(s.,.)` = `.`, `(s/,/)` = `/`, `(s|,|)` = `|` 등 임의 매칭 쌍 가능
- `)` 와의 파싱 모호성은 paired delim 규칙으로 해결 — flag 인자 부분은 닫는 delim까지만 읽음

## Long-form alias — 문법 reserve, v1 구현은 short-form만

- 문법은 `(name1 name2 ...)` 같은 multi-char alphanumeric token도 받아들이도록 reserve (forward compat).
- v1에서 인식하는 flag 이름은 zsh의 short form만 (`s`, `j`, `o`, ...). 그 외 이름은 "unknown flag" error.
- 향후 `(split:,:)`, `(sort)`, `(unique)` 같은 long-form alias 도입 시 v2에서 단순 table 확장으로 가능.
- **미결**: 단일 글자 flag의 juxtaposition (`(uo)` = `u`+`o`)과 multi-char 이름 (`(uo)` = 단일 long name?)의 모호성 처리 규칙. 잠정: 단일 글자 연접은 zsh 호환을 위해 항상 individual flag로 해석, multi-char 이름은 구분자 (공백/콤마) 필수.

## `(e)` re-evaluation — `-secure` modifier로 통제

- 일반 모드 (POSIX-aware, default 등): `(e)` 가용. 단 **warning 옵션** 제공 (`set -o warn-unsafe-eval` 같은; 구체 이름 미정).
- **`-secure` postfix가 붙은 모드** (예: `default-secure`, `POSIX-aware-secure`): 해당 scope의 *caller 측* (= 현재 실행 scope)에서 `(e)` 사용 금지. parse/eval 거부.
- 즉 `-secure` 함수 안에서는 `${(e)var}` 사용 불가. 다른 모드의 함수를 호출하는 건 무관 (해당 함수는 자기 모드 따라 동작).

## ksh93 `${!var[@]}` 등 indirect 표기 — 공존

- ksh93 form (`${!var[@]}`, `${!prefix*}`, `${!var}`)과 zsh form (`${(k)var}`, `${(P)var}`) **둘 다 가용**.
- ksh93 form은 ksh93u-strict, POSIX-aware, default 모두에서 가용 (호환).
- zsh form은 POSIX-aware, default에서 가용 (ksh93u-strict는 zsh 확장이라 비활성).
- 새 코드는 zsh form 권장 — 조합성과 압축성이 좋음.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default | (any)-secure |
|---|---|---|---|---|---|---|
| `${(flag)var}` 표기 전체 | × | ✓ | × | ✓ | ✓ | (base 따름) |
| ksh93 `${!var[@]}` 등 | × | ✓ | ✓ | ✓ | ✓ | (base 따름) |
| compound 확장 `(k)/(v)/(t)` | × | ✓ | × | ✓ | ✓ | (base 따름) |
| `(e)` re-evaluation | × | ✓ (+ warning) | × | ✓ (+ warning) | ✓ (+ warning) | **금지** |
| 권장 iteration form | n/a | zsh `(k)` | ksh `${!.@}` | **ksh `${!.@}`** | zsh `(k)` | (base 따름) |

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md.)

**How to apply:** 향후 보안/안전 관련 설계에서 `-secure` modifier가 차단할 항목 목록을 점진적으로 채울 것. expansion flag와 관련된 모든 새 기능은 이 평가 순서와 모드 가용성 표 형식을 따라 명시.
