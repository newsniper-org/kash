---
name: New shell — typeclass system (committed)
description: Scala 3 inspired typeclass — typeclass/instance/requires, lexical dispatch, type inference + assertion 기반 제약, inheritance 폐기
type: project
originSessionId: bab36ea5-ef29-4c12-865a-4fa226524ed1
---
신규 쉘 typeclass 시스템 확정 사항. (관련: project_shell_typeset.md, project_shell_function_scope.md)

## 기본 구조

```sh
# Typeclass 선언 — 시그니처 + default impl
typeclass Showable {
    function show              # 시그니처만 (인스턴스가 반드시 제공)
}

typeclass Eq {
    function eq
}

typeclass Ord requires Eq {    # typeclass inheritance
    function lt
    function le { _.eq $1 || _.lt $1; }   # default impl, 다른 메소드 호출
    function gt { ! _.le $1; }
    function ge { ! _.lt $1; }
}

# Instance 선언
instance Showable for Int {
    function show { print "$_"; }
}

instance Ord for Int {
    function eq { (( _ == $1 )); }
    function lt { (( _ < $1 )); }
    # le, gt, ge는 default impl 자동 상속
}

# 사용 — dispatch는 runtime type 기반
x=5
echo $(x.show)
```

## Method dispatch resolution

`value.method` 호출 시:
1. 값 자신의 type 메소드 (`typeset -T` 정의된 것)
2. Lexical scope에서 보이는 typeclass instances
3. 못 찾으면 runtime error: "method `<name>` not found for type <Type>"

같은 메소드명이 여러 typeclass의 instance에 존재 시 → ambiguous, runtime error. 명시적 해소: `TypeclassName.method value args` form (예: `Eq.eq $a $b`).

## Coherence

- 같은 (Typeclass, Type) 페어의 instance가 여러 lexical scope에 존재 시: **innermost wins** (lexical).
- 같은 scope에 중복 정의: parse error.

## Scope rules

- `typeclass`, `instance` 선언은 함수처럼 lexically scoped.
- File top → 전체 file에서 가용.
- 함수 안 → 그 함수 내에서만 가용 (정적 스코프와 정합).
- Sourced file의 선언 → caller scope에 들어옴.

## Type inference + assertion (annotation 대체)

함수 정의 문법은 절대 건드리지 않음. Type annotation 대신:

- **Inference**: 런타임에 값의 type을 보고 자동 dispatch. 정적 분석 없음.
- **Assertion**: 함수 body에 명시적 제약 표현.

### 새 test 연산자 (`[[ ]]` 내부)
- `[[ x -is TypeName ]]` — type 정확 일치 (`${(t)x} == TypeName`)
- `[[ x -satisfies TypeclassName ]]` — x의 type이 해당 typeclass의 instance 보유

### `assert` builtin
```sh
assert <expression>
```
- expression이 false → error로 exit. error message는 expression source.
- POSIX-strict/-aware: assert 자체가 신규 builtin이라 `:` no-op로 대체 또는 비활성 (모드별 결정 필요).
- default / -secure: 정상 동작. `-secure`에서는 비활성/우회 옵션 없음.

### 사용 패턴
```sh
function compare(a, b) {
    [[ $a -satisfies Ord ]] || die "a must satisfy Ord"
    [[ $b -satisfies Ord ]] || die "b must satisfy Ord"
    [[ $(${(t)a}) == $(${(t)b}) ]] || die "type mismatch"
    
    a.lt $b
}

# 또는 assert로 더 간결
function compare(a, b) {
    assert [[ $a -satisfies Ord ]]
    assert [[ $b -satisfies Ord ]]
    assert [[ ${(t)a} == ${(t)b} ]]
    
    a.lt $b
}
```

## Inheritance 폐기 (전 결정 뒤집기)

typeset 확장의 "inheritance/composition" 항목 폐기. typeclass가 inheritance의 use case를 대체 (행위 공유, 다형성, 코드 재사용, 인터페이스 강제 전부).

남은 OOP 확장 — 3종:
1. Constructor/destructor (dunder methods)
2. Private 멤버
3. Static / class 멤버

(원래 4종에서 inheritance 빠짐 → 3종)

## v1 보류 (Scala 3에 있지만 미도입)

- Generic types in instances (`instance Showable for List[T] when T: Showable`)
- Type 파라미터 함수 (`function printAll[T: Showable]`)
- Multi-method dispatch
- Implicit conversion (`Conversion[A, B]`)
- Type-level introspection (`${(I)var}` 등)
- Function parameter type annotation (대체: type inference + assertion)

## 모드별 가용성

| 기능 | POSIX-strict | POSIX-aware | ksh93u-strict | ksh93u-aware | default |
|---|---|---|---|---|---|
| `typeclass` 선언 | × | ✓ | × | ✓ | ✓ |
| `instance` 선언 | × | ✓ | × | ✓ | ✓ |
| Typeclass method dispatch | × | ✓ | × | ✓ | ✓ |
| `requires` (typeclass inheritance) | × | ✓ | × | ✓ | ✓ |
| `[[ -is ]]`, `[[ -satisfies ]]` | × | ✓ | × | ✓ | ✓ |
| `assert` builtin | × | ✓ | × | ✓ | ✓ |

## 미결

(All initial 미결 resolved in project_kash_sweep_v1.md.)

**How to apply:** typeclass 관련 후속 설계 (특히 v1 보류 항목 중 일부 도입 검토 시) 이 메모리의 dispatch 규칙과 coherence 정책을 baseline으로. inheritance가 폐기됐음을 항상 기억할 것 — 누군가 "subclass" 패턴을 요청하면 typeclass + composition으로 풀어야 함.
