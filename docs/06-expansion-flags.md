# Parameter Expansion Flags

zsh의 `${(flags)var}` 표기를 차용해 셸 내부에서 awk/sed/cut/sort 등을 흡수.

## 채택 범위 — zsh 전체 + compound 확장

zsh의 *모든* expansion flag 채택. Curated subset 가는 순간 사용자가 매번 확인해야 하므로 학습 부담 오히려 증가.

### 카테고리
- 분할/결합 `(s/j/f/F/z)`
- 정렬/중복 `(o/O/n/i/u)`
- 인용 `(q/qq/qqq/qqqq/Q)`
- 대소문자 `(L/U/C)`
- 간접/메타 `(P/t/@/e/D)`
- assoc `(k/v/kv)`
- 경로 modifier `(:h/:t/:r/:e)`
- 기타 `(g/V/c/#/%)`

### Compound 확장
- `${(k)compound}` → 멤버 이름 리스트 (assoc 키와 의미 통일)
- `${(v)compound}` → 멤버 값 리스트
- `${(kv)compound}` → key/value interleaved
- `${(t)compound.member}` → 그 멤버의 type 문자열 ("scalar", "compound", "indexed", "assoc")

ksh93의 `${!person.@}`는 ksh93u-strict 모드에서만 보존, default는 `${(k)person}`로 통일.

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

## Flag 인자 구분자

Perl-style paired delim. `(s:,:)` = `:`, `(s.,.)` = `.`, `(s/,/)` = `/`, `(s|,|)` = `|` 등 임의 매칭 쌍 가능.

## Long-form alias — 문법 reserve, v1 short-form만

- 문법은 `(name1 name2 ...)` 같은 multi-char alphanumeric token도 받아들이도록 reserve.
- v1에서 인식하는 flag 이름은 zsh의 short form만. 그 외 이름은 "unknown flag" error.
- 향후 `(split:,:)`, `(sort)`, `(unique)` 같은 long-form alias 도입 가능.

## `(e)` re-evaluation — `-secure` modifier로 통제

- 일반 모드: `(e)` 가용. **warning 옵션** 제공 (`set -o warn-unsafe-eval` 같은; 구체 이름 미정).
- **`-secure` postfix 모드**: 해당 scope의 *caller 측* (= 현재 실행 scope)에서 `(e)` 사용 금지. parse/eval 거부.

## ksh93 indirect 표기 — 공존

- ksh93 form (`${!var[@]}`, `${!prefix*}`, `${!var}`)과 zsh form (`${(k)var}`, `${(P)var}`) 둘 다 가용.
- ksh93 form: ksh93u-strict, POSIX-aware, default 모두에서 가용.
- zsh form: POSIX-aware, default (ksh93u-strict는 zsh 확장이라 비활성).
- 새 코드는 zsh form 권장.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default | (any)-secure |
|---|---|---|---|---|---|---|
| `${(flag)var}` 표기 전체 | × | ✓ | × | ✓ | ✓ | (base 따름) |
| ksh93 `${!var[@]}` 등 | × | ✓ | ✓ | ✓ | ✓ | (base 따름) |
| compound 확장 `(k)/(v)/(t)` | × | ✓ | × | ✓ | ✓ | (base 따름) |
| `(e)` re-evaluation | × | ✓ (+ warning) | × | ✓ (+ warning) | ✓ (+ warning) | **금지** |
| 권장 iteration form | n/a | zsh `(k)` | ksh `${!.@}` | **ksh `${!.@}`** | zsh `(k)` | (base 따름) |

## 미결

- `warn-unsafe-eval` 같은 warning 옵션의 구체 이름
- multi-char alias parsing의 모호성 규칙 (`(uo)` = `u`+`o` vs 단일 long name)
- `-secure` modifier가 `(e)` 외에 추가로 차단할 항목
- `(z)` shell-tokenize, `(g::)` print-style escape의 `-secure` 하 처리
