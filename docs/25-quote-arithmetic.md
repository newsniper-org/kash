# Quote Handling and Arithmetic

## Quote handling

### POSIX core (모든 모드)
- `'literal'` — expansion 없음
- `"$var and \$escape"` — param/cmd/arith subst
- `\X` — single-char escape outside quotes
- `\` line continuation

### `$'...'` ANSI-C — 모든 모드 채택

POSIX.1-2024 정식 채택. Escape 지원 (bash superset):
- `\n`, `\t`, `\r`, `\b`, `\a`, `\f`, `\v`, `\e`/`\E`, `\\`, `\'`, `\"`, `\?`
- `\xHH` (hex byte), `\nnn` (octal)
- `\uHHHH`, `\UHHHHHHHH` (bash Unicode form)
- `\u{HEX}` (ksh93 Unicode form, 동시 채택)

### `$"..."` gettext — kash 미지원, transpiler가 처리

bash/ksh93의 `$"..."`은 native syntax 아님. **script transpiler의 gettext plugin**이 `$(gettext "...")` 같은 호출로 변환.

## Arithmetic

### Surface forms
- `$((expr))` — POSIX, 모든 모드
- `((expr))` — 명령 form, ksh/bash 확장
- `let expr` — builtin
- `${(#)var}` — expansion flag

### Operators

C-style 전체: 산술 (`+ - * / % **`), 단항 (`+ - ! ~ ++/--`), shift (`<< >>`), 비교, 비트, 논리, 삼항, 할당 (compound 포함), comma. C precedence.

산술 컨텍스트에서 변수는 `$` 없이 참조 가능.

### Base 표기
`$((BASE#NUM))` — `2#1010`, `16#FF` 등

## Primitive numeric types

`typeset -T` 사용자 정의와 별개의 built-in. lowercase 컨벤션.

### 정수 (10개)
- `int8`, `int16`, `int32`, **`int64`** (기본), `int128`
- `uint8`, `uint16`, `uint32`, `uint64`, `uint128`

### 부동소수 (5개)
- `float16`, `float32`, **`float64`** (기본), `float128`
- `bfloat16` (Google Brain — ML 친화)

### 복소수 (5개)
- `complex32` = 2 × float16
- `complex64` = 2 × float32
- **`complex128`** (기본) = 2 × float64
- `complex256` = 2 × float128
- `bcomplex32` = 2 × bfloat16 (ML 친화)

### Alias 없음
짧은 alias는 사용자 변수명과 충돌 위험 — 항상 full name (`int64`, `float32` 등).

### ksh93 flag 호환

| 전통 | 새 type |
|---|---|
| `typeset -i x` | `typeset int64 x` |
| `typeset -E x` / `-F x` | `typeset float64 x` |

⚠️ `typeset -i16 x` (ksh93: base 16) vs `typeset int16 x` (16-bit) — 의미 다름.

- default: type-name canonical
- ksh93u-strict/aware: ksh93 의미 유지
- transpiler가 매핑

### 선언 예시
```kash
int32 a=42
uint8 byte=0xFF
float32 x=3.14
bfloat16 weight=0.5
complex128 z=(re=1.0 im=2.0)
bcomplex32 w=(re=0.5 im=0.5)
```

## Type promotion — C-style implicit

- 작은 → 큰
- 정수 → 부동소수
- 실수 → 복소수
- bfloat16/float16 → float32+ (promotion 시)
- bcomplex/complex → complex (bcomplex narrow)

### 명시 변환
```kash
int8 small=$((int8(big)))
float32 f=$((float32(big)))
complex128 z=$((complex128(big)))
```

## Overflow

- 기본: wrap (C-style)
- `warn-integer-overflow` 옵션 (warn-* 패밀리, `-secure` lock)

부동소수: IEEE 754 (NaN/Inf, exception 없음).

## Complex arithmetic

산술 operator는 complex 피연산자 자동 인식:
```kash
complex128 r=$((a * b))
```

Math 함수: `cabs`, `carg`, `creal`, `cimag`, `cconj`, c-prefix 일반화.

## Math library

ksh93 baseline + complex 확장.

- 삼각/쌍곡/지수/로그
- sqrt/cbrt/hypot
- floor/ceil/round/trunc/rint
- abs/fabs/copysign
- fmod/remainder
- isnan/isinf/isfinite
- frexp/ldexp/modf
- complex: cabs/carg/creal/cimag/cconj

## 상수
`M_E`, `M_PI`, `M_PI_2`, `M_PI_4`, `M_SQRT2`, `M_LN2`, `M_LN10`, `M_LOG2E`, `M_LOG10E` (모두 float64).

## Random / 시간

| 전통 | kash form |
|---|---|
| `$RANDOM` | `${.sh.random}` |
| `$SECONDS` | `${.sh.seconds}` |
| `$EPOCHREALTIME` | `${.sh.epoch.real}` |
| `$EPOCHSECONDS` | `${.sh.epoch.seconds}` |

둘 다 가용.

## 모드별 가용성 (numeric)

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `$((expr))` 정수 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `((expr))` command | × | ✓ | ✓ | ✓ | ✓ |
| int64 기본 | × | ✓ | ✓ | ✓ | ✓ |
| Wide integer (int8/16/32/128, uint*) | × | × | × | ✓ | ✓ |
| float64 | × | × | ✓ | ✓ | ✓ |
| Specialty float (16/bfloat16/128) | × | × | × | ✓ | ✓ |
| Complex/bcomplex | × | × | × | ✓ | ✓ |
| Math library | × | × | ✓ | ✓ | ✓ |
| `warn-integer-overflow` | n/a | off | off | off | off (`-secure`: on lock) |

## 미결

- Narrow type round 정책 정확한 spec
- Imaginary unit 리터럴 (현재: compound만)
- Type 충돌 코너 케이스
- Math 함수 이름과 변수 충돌
