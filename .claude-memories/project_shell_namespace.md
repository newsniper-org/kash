---
name: New shell — namespace system (committed)
description: ksh93 baseline + use namespace import + typeclass instance scoping + reopening + privacy 컨벤션
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 namespace 시스템 확정 사항. (관련: project_shell_compound_vars.md, project_shell_typeset.md, project_shell_typeclass.md, project_shell_modes.md)

## 기본 구조 (ksh93 baseline)

```sh
namespace utils {
    typeset version="1.0"
    function log_info { print "[INFO] $1"; }
    function log_error { print "[ERROR] $1" >&2; }
}

# 외부 접근 — 항상 leading `.` (full path)
.utils.log_info "hello"
echo ${.utils.version}

# 내부에서는 short form 가능 (namespace가 search path에 자동 포함)
namespace utils {
    function show_version { print "$version"; }   # `version` 직접 참조 OK
}
```

## `.` 어휘 충돌 해소 (compound member access와 직교)

| 표기 | 의미 |
|---|---|
| `.foo.x` | namespace `.foo`의 `x` |
| `var.x` | compound `var`의 멤버 `x` |
| `.foo.var.x` | namespace `.foo`의 compound `var`의 멤버 `x` |

규칙: **leading `.`이 namespace, 변수명 중간의 `.`은 compound member.** 항상 명확히 구분.

`.sh.*` (시스템 namespace), `.sh.mode`, `.sh.match`, `.sh.value` 등 우리가 보존한 ksh93 reserved namespace도 동일 메커니즘.

## Namespace에 담길 수 있는 것

거의 모든 declaration:
- 변수 (`typeset`)
- 함수 (`function name { }`)
- 사용자 정의 type (`typeset -T`)
- Compound vars
- Discipline functions (`function .ns.var.set { ... }`)
- **Typeclasses와 instances** (아래 별도 정책)
- Nested namespaces (`namespace foo { namespace bar { ... } }` → `.foo.bar.x`)

## Reopening 허용

```sh
namespace utils { function a { ... } }
# ... 다른 파일 또는 다른 위치 ...
namespace utils { function b { ... } }    # .utils.a와 .utils.b 둘 다 존재
```

ksh93 정합. 큰 namespace를 여러 파일로 쪼개기에 유용.

## Import mechanism — `use namespace`

ksh93에 없는 신규 확장:

```sh
use namespace utils                   # 모든 .utils.* 심볼을 현재 scope에 short name으로
use namespace utils as u              # alias — .u.log_info
use .utils.log_info                   # 단일 심볼만
use .utils.{log_info,log_error}       # brace expansion으로 복수 심볼 선택
```

**`use`는 lexical scope에 작용** — 함수 안의 `use`는 그 함수 안에서만 효력. 함수 스코프와 mode scoping과 정합.

## Privacy — v1은 컨벤션 + import 시 자동 제외

- `_namespace_internal` 같은 underscore prefix 컨벤션으로 외부 사용 비권장 표시
- `use namespace foo`로 import 시 `_*` 심볼은 **자동 제외** (Python `__all__` 유사)
- 명시적 import (`use .foo._helper`)는 가능 — 의도 표명
- 언어 차원 `private` access modifier는 typeset OOP 확장에서 후속 도입 (그때 namespace에도 일관 적용)

## Typeclass instance의 namespace scoping (가장 중요한 결정)

Scala 3의 `given` import 모델과 동일 — **instance는 namespace에 묶이고, scope에서 보일 때만 dispatch에 참여**.

```sh
namespace foo {
    typeclass Showable { function show; }
    instance Showable for Int { function show { print "FOO:$_"; } }
}

namespace bar {
    typeclass Showable { function show; }
    instance Showable for Int { function show { print "BAR:$_"; } }
}

# .foo.Showable과 .bar.Showable은 별개 typeclass (이름 같지만 namespace 다름)
x=5
[[ $x -satisfies .foo.Showable ]]      # true
[[ $x -satisfies .bar.Showable ]]      # true
.foo.Showable.show $x                   # "FOO:5"
.bar.Showable.show $x                   # "BAR:5"

# bare dispatch는 lexical scope에서 보이는 instance만 사용
use namespace foo
echo $(x.show)                          # "FOO:5" — foo의 Showable만 import
```

Typeclass 메모리의 "lexical wins" 정책과 정합. `use`로 어떤 instance를 가져왔는지에 따라 dispatch 결정.

## 파일과 namespace의 관계

- **`source PATH` (= `. PATH`)**: ksh93 style — 파일 내용을 caller scope에서 실행. namespace 자동 처리 없음.
- **`use namespace .foo.bar`**: module system — file path → namespace 자동 매핑 (project_kash_module_resolution.md). 자동으로 `namespace .foo.bar { ... }` 로 wrap.

두 메커니즘 분리 — generic include는 `source`, module load는 `use namespace`.

## Namespace 안의 mode declaration

- 기본: enclosing scope의 mode 상속
- 명시적: `namespace foo { mode -L posix-aware { ... } }` 가능 — namespace block 안에서 mode 변경
- mode 시스템의 lexical scoping과 자동 정합

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

남은 v2+ 항목:
- 언어 차원 `private` access modifier 도입 — 현재는 `_prefix` 컨벤션, 향후 typeset OOP 확장과 함께 (v2+)

기타 모두 project_kash_sweep_v1.md에서 해소 — `use` 충돌은 strict error / default shadow+warning, brace는 `use .ns.{*}` 명시 form, re-export는 `re-export` 구문, versioning은 path 활용, cyclic은 lazy load + cycle detection error.

**How to apply:** 새 declaration form (예: 향후 추가될 `private`, `static`, dunder methods)은 namespace 내부에서도 동일하게 동작해야 함. typeclass dispatch는 항상 lexical scope를 거쳐 결정 (namespace import 통해 가시화된 것만 후보).
