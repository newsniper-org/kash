# Namespace System

ksh93 baseline + 신규 import 메커니즘.

## 기본 구조

```sh
namespace utils {
    typeset version="1.0"
    function log_info { print "[INFO] $1"; }
    function log_error { print "[ERROR] $1" >&2; }
}

# 외부 접근 — 항상 leading `.` (full path)
.utils.log_info "hello"
echo ${.utils.version}

# 내부에서는 short form 가능
namespace utils {
    function show_version { print "$version"; }
}
```

## `.` 어휘 충돌 해소

leading `.`이 namespace, 변수명 중간의 `.`은 compound member.

| 표기 | 의미 |
|---|---|
| `.foo.x` | namespace `.foo`의 `x` |
| `var.x` | compound `var`의 멤버 `x` |
| `.foo.var.x` | namespace `.foo`의 compound `var`의 멤버 `x` |

`.sh.*` (시스템 namespace), `.sh.mode` 등도 동일 메커니즘.

## Namespace에 담길 수 있는 것

거의 모든 declaration:
- 변수 (`typeset`)
- 함수 (`function name { }`)
- 사용자 정의 type (`typeset -T`)
- Compound vars
- Discipline functions
- Typeclasses와 instances
- Nested namespaces

## Reopening 허용

```sh
namespace utils { function a { ... } }
namespace utils { function b { ... } }    # 둘 다 존재
```

ksh93 정합. 여러 파일로 쪼개기에 유용.

## Import — `use namespace`

ksh93에 없는 신규 확장:

```sh
use namespace utils                   # 모든 .utils.* 심볼을 short name으로
use namespace utils as u              # alias
use .utils.log_info                   # 단일 심볼
use .utils.{log_info,log_error}       # brace expansion으로 복수
```

**`use`는 lexical scope에 작용** — 함수 안의 `use`는 그 함수 안에서만.

## Privacy (v1)

- `_prefix` 컨벤션으로 외부 사용 비권장 표시
- `use namespace foo`로 import 시 `_*` 심볼 자동 제외 (Python `__all__` 유사)
- 명시적 import (`use .foo._helper`)는 가능
- 언어 차원 `private`는 typeset OOP 확장에서 후속 도입

## Typeclass instance의 namespace scoping

Scala 3 `given` import 모델 — instance는 namespace에 묶이고, scope에서 보일 때만 dispatch에 참여.

```sh
namespace foo {
    typeclass Showable { function show; }
    instance Showable for Int { function show { print "FOO:$_"; } }
}

namespace bar {
    typeclass Showable { function show; }
    instance Showable for Int { function show { print "BAR:$_"; } }
}

# 별개 typeclass (이름 같지만 namespace 다름)
x=5
.foo.Showable.show $x                   # "FOO:5"
.bar.Showable.show $x                   # "BAR:5"

use namespace foo
echo $(x.show)                          # "FOO:5" — foo의 Showable만 import
```

Typeclass의 "lexical wins" 정책과 정합.

## 파일과 namespace

- 파일은 namespace와 무관. `namespace foo { ... }`을 명시적 선언.
- `source` 의미: 파일 내용이 caller scope에 그대로 들어옴.
- 향후 module system은 별도 결정.

## Namespace 안의 mode declaration

- 기본: enclosing scope의 mode 상속
- 명시적: `namespace foo { mode -L posix-aware { ... } }` 가능

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `namespace` 선언 | × | ✓ | ✓ | ✓ | ✓ |
| Nested namespace | × | ✓ | ✓ | ✓ | ✓ |
| Namespace reopening | × | ✓ | ✓ | ✓ | ✓ |
| `use namespace` (import) | × | ✓ | × (ksh93에 없음) | ✓ | ✓ |
| 부분 import (`use .ns.x`) | × | ✓ | × | ✓ | ✓ |
| Typeclass의 namespace scoping | × | ✓ | × | ✓ | ✓ |

## 미결

- `use namespace`의 충돌 처리 (current scope에 이미 같은 이름 존재 시)
- Brace expansion 안의 wildcard (`use .ns.*` 형태)
- Re-export 메커니즘
- Namespace versioning / qualified path
- Cyclic namespace 처리
- 언어 차원 `private` 도입 시점
