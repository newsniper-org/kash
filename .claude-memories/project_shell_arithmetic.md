---
name: kash — arithmetic and numeric types (committed)
description: POSIX/ksh93 arithmetic + 확장 primitive numeric types (int*, uint*, float*, bfloat16, complex*), math library, complex 산술
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash arithmetic 시스템 + numeric type set 확정 사항. (관련: project_shell_typeset.md, project_shell_modes.md)

## Surface forms

| Form | 의미 | POSIX |
|---|---|---|
| `$((expr))` | 산술 expansion (값 반환) | ✓ |
| `((expr))` | 산술 command (exit code = expr non-zero?) | × (ksh/bash 확장) |
| `let expr` | builtin (== `((expr))`) | × |
| `${(#)var}` | expansion flag 평가 | zsh — 채택 |
| `$((BASE#NUM))` | base 표기 (예: `2#1010`) | ksh93/bash |

## Operators — ksh93/bash union

전통 C-style 전체:
- 산술: `+ - * / % **`
- 단항: `+ - ! ~ ++/-- (pre/post)`
- Shift: `<< >>`
- 비교: `< <= > >= == !=`
- 비트: `& ^ |`
- 논리: `&& ||`
- 삼항: `? :`
- 할당: `= += -= *= /= %= <<= >>= &= ^= |= **=`
- Comma: `,`
- C precedence

산술 컨텍스트(`$((expr))`)에서 변수는 `$` 없이 참조 가능.

## Primitive numeric type set

`typeset -T` 사용자 정의와 별개의 **built-in primitive type**. lowercase 컨벤션 (custom struct는 PascalCase, primitive는 lowercase).

### 정수 (10개)
- 부호 있음: `int8`, `int16`, `int32`, **`int64`** (기본), `int128`
- 부호 없음: `uint8`, `uint16`, `uint32`, `uint64`, `uint128`

### 부동소수 (5개)
- `float16`, `float32`, **`float64`** (기본), `float128`
- `bfloat16` (Google Brain bfloat16 — ML 친화)

### 복소수 (5개)
- `complex32` = 2 × float16
- `complex64` = 2 × float32
- **`complex128`** (기본) = 2 × float64
- `complex256` = 2 × float128
- `bcomplex32` = 2 × bfloat16 (ML 친화)

### Alias — *없음*
짧은 `int`/`uint`/`float`/`complex` aliases는 **드랍**. 사용자 변수명과 충돌 위험. 항상 full name 사용 (`int64`, `uint8`, `float32` 등).

### ksh93 flag 호환

| 전통 flag | 새 type |
|---|---|
| `typeset -i x` | `typeset int64 x` |
| `typeset -E x` | `typeset float64 x` (exponential 출력 attribute 별도) |
| `typeset -F x` | `typeset float64 x` (fixed 출력 attribute 별도) |

⚠️ **충돌**: ksh93 `typeset -i16 x`는 *base 16 정수* (16진수 출력). 우리 `typeset int16 x`는 16-bit 정수. 의미 다름.

해소:
- **default**: type-name form canonical (`int16` = 16-bit). ksh의 base-flag form은 deprecation warning.
- **ksh93u-strict/aware**: ksh93 의미 유지 (`-i16` = base 16).
- Transpiler가 ksh93 `-i N` → `int64` + 16진 출력 attribute로 변환.

### 선언 form 예시

```kash
int32 a=42
uint8 byte=0xFF
float32 x=3.14
bfloat16 weight=0.5
complex128 z=(re=1.0 im=2.0)
bcomplex32 w=(re=0.5 im=0.5)
```

## Type promotion — C-style implicit

- 작은 → 큰 자동 (int8 + int64 = int64)
- 정수 → 부동소수 (int64 + float64 = float64)
- 실수 → 복소수 (float64 + complex128 = complex128)
- 부호 다름 (int8 + uint8): u-side로 (strict 모드 명시 변환 권장 — 별도 결정)
- bfloat16 / float16 — 다른 mantissa, promotion 시 float32 이상으로
- bcomplex / complex 간 promotion: complex 쪽으로 (bcomplex는 ML-특화로 narrow)

### 명시 변환
```kash
int64 big=5
int8 small=$((int8(big)))            # type-name function form
float32 f=$((float32(big)))
complex128 z=$((complex128(big)))    # 실수만 → real part
```

## Overflow 처리

- 기본: wrap (C-style, ksh93 정합)
- 신규 옵션 **`warn-integer-overflow`** (warn-* 패밀리 합류)
- `-secure`에 추가 lock (warn-* 일관)

부동소수: IEEE 754 표준 (NaN, Inf 발생, exception 안 함).

## Complex arithmetic

산술 operator는 complex 피연산자 자동 인식:
```kash
complex128 a=(re=1 im=2)
complex128 b=(re=3 im=4)
complex128 r=$((a * b))             # -5+10i
```

### Math library — complex 일반화

기존 함수 (sqrt, sin, cos, log 등)는 complex 인자 들어오면 complex 결과.

신규 complex-specific 함수:
- `cabs(z)` — magnitude
- `carg(z)` / `cphase(z)` — argument
- `creal(z)` / `cimag(z)` — 실/허
- `cconj(z)` — 복소공역

## Math library — 전체 set

ksh93 baseline + complex 확장:
- 삼각: sin/cos/tan/asin/acos/atan/atan2 (complex 시 c-suffix)
- 쌍곡: sinh/cosh/tanh/asinh/acosh/atanh
- 지수/로그: exp/log/log2/log10/pow
- 거듭제곱/근: sqrt/cbrt/hypot
- 반올림: floor/ceil/round/trunc/rint
- 절대값/부호: abs/fabs/copysign/signbit
- 모듈러: fmod/remainder
- 분류: isnan/isinf/isfinite/isnormal
- 분수: frexp/ldexp/modf
- complex: cabs/carg/creal/cimag/cconj + c-prefix 일반화

## 상수 (math constants)

ksh93 정합 (모두 float64):
- `M_E`, `M_PI`, `M_PI_2`, `M_PI_4`, `M_1_PI`, `M_2_PI`, `M_SQRT2`, `M_LN2`, `M_LN10`, `M_LOG2E`, `M_LOG10E`

## Random / 시간 변수

| 전통 | kash form (`.sh.*` compound) | 의미 |
|---|---|---|
| `$RANDOM` | `${.sh.random}` | 16-bit pseudo-random (ksh93 정합) |
| `$SECONDS` | `${.sh.seconds}` | 셸 시작 후 경과 초 |
| `$EPOCHREALTIME` (bash 5+) | `${.sh.epoch.real}` | Unix epoch float |
| `$EPOCHSECONDS` (bash 5+) | `${.sh.epoch.seconds}` | Unix epoch int |

둘 다 가용. transpiler 매핑.

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `$((expr))` 정수 | ✓ | ✓ | ✓ | ✓ | ✓ |
| `((expr))` command | × | ✓ | ✓ | ✓ | ✓ |
| `int64` 기본 (`typeset -i`) | × | ✓ | ✓ | ✓ | ✓ |
| Wide integer (int8/16/32/128, uint8-128) | × | × | × | ✓ | ✓ |
| 부동소수 (float64) | × | × | ✓ | ✓ | ✓ |
| Specialty float (float16/bfloat16/128) | × | × | × | ✓ | ✓ |
| Complex (complex*, bcomplex32) | × | × | × | ✓ | ✓ |
| Math library 전체 | × | × | ✓ | ✓ | ✓ |
| `warn-integer-overflow` | n/a | off | off | off | off (`-secure`: on lock) |

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md — round은 IEEE 754 round-to-nearest-even, imaginary literal `1+2i` 도입, int128+uint128 promotion은 helper 함수로 사용자 처리, 함수/변수 충돌은 컨텍스트 기반.)

**How to apply:** numeric 관련 후속 결정 (rational/decimal/bignum 등 추가, vector type 등) 시 위 type set 베이스에 추가. lowercase primitive 컨벤션 유지.
