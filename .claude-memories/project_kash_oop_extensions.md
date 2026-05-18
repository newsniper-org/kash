---
name: kash — OOP extensions for typeset -T (committed)
description: Dunder는 lifecycle 2종만 (__init/__del), 모든 capability(__str/__repr/__hash/__call/__iter 등)는 typeclass로, private은 class-private, static은 TypeName.member 접근
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
kash `typeset -T` OOP 확장 확정 사항. (관련: project_shell_typeset.md, project_shell_typeclass.md, project_shell_function_scope.md)

## Dunder methods — lifecycle 2종만

| Dunder | 호출 시점 | 인자 | 의미 |
|---|---|---|---|
| `__init` | 인스턴스 생성 시 | 생성 form 따라 | 초기화/검증 |
| `__del` | unset 또는 scope 종료 시 | 없음 | 정리 (close fd, release resource 등) |

**핵심 원칙**: dunder는 *type-inherent lifecycle hook*만. 모든 *capability* (행위, 변환, 연산자)는 typeclass로 처리.

### 기존 제안에서 제거된 dunder들 → 전부 typeclass로

| 후보 dunder | 대신 사용할 typeclass |
|---|---|
| `__str` (string conversion) | `Showable` (`function show`) |
| `__repr` (debug repr) | `Showable` (extended) 또는 별도 `Reprable` typeclass |
| `__hash` (해시) | `Hashable` typeclass |
| `__call` (callable instance) | `Callable` typeclass |
| `__iter` (iteration) | `Iterable` typeclass |
| `__eq` / `__lt` / `__gt` 등 | `Eq` / `Ord` (이미 typeclass 결정) |

이유:
- lifecycle은 type 정의의 일부 (init/del 없는 type은 본질적으로 다른 것)
- capability는 *외부에서 추가 가능*해야 (Rust trait, Scala typeclass 패턴) — retroactive ad-hoc polymorphism
- 두 메커니즘이 같은 일을 하면 사용자가 어디다 쓸지 매번 결정해야 함 → 학습 부담

### 표준 typeclass library (향후 별도 commit)

표준 typeclass들 (Eq, Ord, Showable, Hashable, Callable, Iterable, Reprable 등)을 어떤 namespace에 어떻게 제공할지는 별도 결정. 잠정 `.kash.std.*` 또는 prelude처럼 always-loaded.

## 인스턴스 생성 form 3가지

| Form | 동작 |
|---|---|
| `MyType x` | allocate → `__init` 호출 (인자 없음, 정의 있으면) |
| `MyType x(arg1, arg2)` | allocate → `__init arg1 arg2` 호출 |
| `MyType x=(field=val ...)` | allocate → **직접 필드 할당, `__init` bypass** |

세 번째 form은 raw construction — 역직렬화, 복제 등 `__init` 검증 우회가 의도된 경우용 escape hatch.

## Private 멤버 — 언어 차원 access modifier

```kash
typeset -T Stack=(
    private function _validate { ... }
    private typeset _cached_top
    
    function push {
        _._validate          # 인스턴스 메소드 안 — 가능
    }
)

Stack s
s.push apple       # OK
s._validate         # ERROR
echo $s._cached_top # ERROR
```

**Class-private 정책** (Java 비슷):
- 접근 허용: type의 인스턴스 메소드, discipline function, static 메소드
- 접근 거부: 외부 코드
- 모듈/namespace-private 아님 — type 단위가 boundary
- 검사: parse-time 가능하면 그때, 아니면 runtime

## Static / class 멤버

```kash
typeset -T Counter=(
    typeset -i count=0           # instance
    static typeset -i total=0    # class-level
    
    function __init {
        ((Counter.total++))
        _.count=${1:-0}
    }
    
    function __del {
        ((Counter.total--))
    }
    
    static function reset_all {
        Counter.total=0
    }
)
```

**Access pattern**:
- `${TypeName.member}` — class-level 필드
- `TypeName.method args` — static method 호출 (인스턴스 없이)

**Static 메소드의 `_`**:
- 정의되지 않음 (인스턴스 reference 없음)
- 잘못 참조 시 unset (strict 모드에서 error)

**Type/변수 모호성**:
- 컨벤션: type은 PascalCase, 변수는 camel/snake (강제 아님, 권장)
- 동일 이름 충돌 시 compound access 컨텍스트에서 type 우선
- 명시적 escape: `${.kash.types.Foo.bar}`

## 통합 예시

```kash
typeset -T Connection=(
    typeset host=""
    typeset -i port=0
    typeset -i fd=-1
    static typeset -A active
    
    function __init {
        _.host=$1; _.port=$2
        coproc _c tcp-connect "$_.host" "$_.port"
        _.fd=${_c[0]}
        Connection.active[$_.host:$_.port]=$_
    }
    
    function __del {
        _.close
        unset 'Connection.active[$_.host:$_.port]'
    }
    
    function send { printf '%s' "$1" >&$_.fd; }
    function recv { local line; read -r line <&$_.fd; print -- "$line"; }
    private function close { [[ $_.fd -ge 0 ]] && exec {_.fd}>&-; }
    
    static function close_all {
        local key
        for key in "${!Connection.active[@]}"; do
            unset 'Connection.active[$key]'
        done
    }
)

# 별도로 Showable instance 제공 (typeclass)
instance Showable for Connection {
    function show {
        print "Connection($_.host:$_.port, fd=$_.fd)"
    }
}
```

`__str` dunder 대신 `Showable for Connection` 구현 — 두 메커니즘이 분리되어 *외부에서도* show 동작 추가 가능 (type 정의를 수정하지 않고).

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| Dunder (`__init`, `__del`) | × | ✓ | × (ksh93에 없음) | ✓ | ✓ |
| `MyType x(args)` constructor form | × | ✓ | × | ✓ | ✓ |
| `private` keyword | × | ✓ | × | ✓ | ✓ |
| `static` keyword | × | ✓ | × | ✓ | ✓ |
| Class-level access (`TypeName.member`) | × | ✓ | × | ✓ | ✓ |

ksh93u-strict는 OOP 확장 전체 비활성 (ksh93에 없음).

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md.)

**How to apply:** 향후 type 시스템 확장 시 *capability는 typeclass, lifecycle은 dunder* 분리 원칙 유지. 새 dunder 추가 제안은 lifecycle 카테고리에 해당하는 경우만.
