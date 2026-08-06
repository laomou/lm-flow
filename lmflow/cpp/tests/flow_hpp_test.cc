// flow.hpp C++ 糖层的独立单元测试 —— 只依赖头文件,不需链接引擎。
//
// 守卫的核心不变量:C++ 算子的任何异常都**不得穿越 extern "C" 回到 Rust 引擎**
// (那是 UB;Rust 的 catch_unwind 接不住 C++ 异常),且 create 失败(返回 nullptr)
// 后 open/process/close 必须安全返回错误而不是 null 解引用。
//
//   g++ -std=c++17 -Wall -Wextra -Werror -Iinclude cpp/flow_hpp_test.cc -o flow_hpp_test

#include <cassert>
#include <cstdio>
#include <cstring>
#include <map>
#include <stdexcept>
#include <string>

#include "lmflow/flow.hpp"

/* ---------- type_id 与 LMFLOW_DECLARE_TYPE_NAME(ADR #22)----------
 * 默认 type_id 取自**修饰名**,那是跨编译器可能不同的东西(GCC/Clang 走 Itanium ABI
 * 一致,MSVC 不同)。`LMFLOW_DECLARE_TYPE_NAME` 是官方逃生口,但此前**没有任何测试
 * 钉住它** —— 于是「宏悄悄变成 no-op」或「哈希算法被改」都不会有人发现,而后果是
 * 跨工具链混用算子时类型校验静默失配。下面几条就是补这个。 */
struct PlainType {
  int x;
};
struct StableType {
  int x;
};
struct AlsoStable {  // 与 StableType 不同的类型,但声明同一个稳定名
  double y;
};

namespace {

// 构造函数抛异常(如打开设备失败):create 必须接住并返回 nullptr。
struct ThrowingCtorKernel : lmflow::Kernel {
  ThrowingCtorKernel() { throw std::runtime_error("ctor boom"); }
  lmflow::Status Process(lmflow::Context&) override { return lmflow::Status::Ok(); }
};

// Process 抛异常:必须转成 LMFLOW_ERR_KERNEL。
struct ThrowingProcessKernel : lmflow::Kernel {
  lmflow::Status Process(lmflow::Context&) override { throw std::runtime_error("process boom"); }
};

struct ThrowingContractKernel : lmflow::Kernel {
  static void GetContract(lmflow::Contract&) { throw std::runtime_error("contract boom"); }
  lmflow::Status Process(lmflow::Context&) override { return lmflow::Status::Ok(); }
};

// 正常算子:Process 不碰 Context,便于纯头文件测试(无需引擎符号)。
struct OkKernel : lmflow::Kernel {
  lmflow::Status Process(lmflow::Context&) override { return lmflow::Status::Ok(); }
};

// 用 LMFLOW_RET_CHECK 的算子:条件不成立时应带着表达式与位置返回失败。
struct RetCheckKernel : lmflow::Kernel {
  bool pass = false;
  lmflow::Status Process(lmflow::Context& cc) override {
    LMFLOW_RET_CHECK(cc, pass);
    return lmflow::Status::Ok();
  }
};

struct RetCheckMsgKernel : lmflow::Kernel {
  lmflow::Status Process(lmflow::Context& cc) override {
    const int ndim = 3;
    LMFLOW_RET_CHECK_MSG(cc, ndim == 4, "only NCHW is accepted");
    return lmflow::Status::Ok();
  }
};

}  // namespace

// 引擎符号打桩:本测试不链引擎,但要验证 SetError 收到的**文本内容**。
// 一个非空的假句柄即可 —— 糖层只把它原样传给下面这个桩。
namespace {
char g_last_error[512];
char g_contract_error[512];
LMFlowContext* const kFakeCtx = reinterpret_cast<LMFlowContext*>(1);
LMFlowContract* const kFakeContract = reinterpret_cast<LMFlowContract*>(1);
struct RegisteredType {
  std::string name;
  size_t size;
  size_t align;
};
std::map<uint64_t, RegisteredType> g_registered_types;
}  // namespace

extern "C" void lmflow_ctx_set_error(const LMFlowContext*, const char* msg) {
  std::snprintf(g_last_error, sizeof(g_last_error), "%s", msg ? msg : "");
}

extern "C" void lmflow_contract_set_error(LMFlowContract*, const char* msg) {
  std::snprintf(g_contract_error, sizeof(g_contract_error), "%s", msg ? msg : "");
}

extern "C" const char* lmflow_last_error() { return g_last_error; }

extern "C" LMFlowStatus lmflow_register_type_descriptor(
    uint64_t type_id, const char* name, size_t size, size_t align) {
  const RegisteredType next{name ? name : "", size, align};
  const auto it = g_registered_types.find(type_id);
  if (it == g_registered_types.end()) {
    g_registered_types.emplace(type_id, next);
    return LMFLOW_OK;
  }
  if (it->second.name == next.name && it->second.size == size && it->second.align == align) {
    return LMFLOW_OK;
  }
  std::snprintf(g_last_error, sizeof(g_last_error), "conflicting custom type descriptor");
  return LMFLOW_ERR_INVALID_ARG;
}

LMFLOW_DECLARE_TYPE_NAME(StableType, "lmflow.test.Stable")
LMFLOW_DECLARE_TYPE_NAME(AlsoStable, "lmflow.test.Stable")

namespace {
void test_type_id() {
  // 1) 声明了稳定名 → id 由**该字符串**决定,与修饰名无关。
  //    常量与 cpp/abi_assert.cc 的 static_assert、tests/abi_layout.rs 的断言同一个数。
  assert(lmflow::TypeId<StableType>() == 0xBFB531B283179309ULL);

  // 2) **证明宏真的生效了**:若它悄悄变成 no-op,这里就会等于修饰名算出的值。
  //    这条是本组测试的关键 —— 只断言 (1) 的话,一个 no-op 宏也可能巧合通过。
  assert(lmflow::TypeId<StableType>() !=
         lmflow::NormalizeTypeId(lmflow::Fnv1a(typeid(StableType).name())));

  // 3) 未声明稳定名的类型:id 就是修饰名的哈希 —— 这正是**跨编译器不稳定**的那一半,
  //    在此显式写明(而不是断言某个具体数值,那会把测试绑死在一种 ABI 上)。
  assert(lmflow::TypeId<PlainType>() ==
         lmflow::NormalizeTypeId(lmflow::Fnv1a(typeid(PlainType).name())));

  // 4) 稳定名即身份:两个**不同的 C++ 类型**声明同一个名字 → 同一个 id。
  //    这是逃生口的定义性质(跨工具链靠名字对齐,而非靠类型),也是它的风险面。
  assert(lmflow::TypeId<StableType>() == lmflow::TypeId<AlsoStable>());

  // 5) 自定义标识不得落进内建区 0..15。
  assert(lmflow::TypeId<PlainType>() >= 16);
  assert(lmflow::TypeId<StableType>() >= 16);

  // 6) 二期类型描述符把名字与布局一起注册。同一声明幂等,同名但布局不同会失败。
  assert(lmflow::RegisterType<StableType>() == LMFLOW_OK);
  assert(lmflow::RegisterType<StableType>() == LMFLOW_OK);
  assert(lmflow::RegisterType<AlsoStable>() == LMFLOW_ERR_INVALID_ARG);

  // 7) 默认类型名跟随编译器 ABI。这里分别钉住当前支持的两套命名规则,防止
  //    `typeid(T).name()` 行为变化后静默改变 type_id。
  //
  // `core/src/packet.rs` 的 `fnv1a_matches_cpp_sugar_layer` 断言的是「Rust 对字符串
  // "i" 的哈希 == 某常量」—— 那只钉住了**哈希函数**,在任何编译器上都同样通过。
  // 它**没有**断言「本编译器的 `typeid(int).name()` 真的是 "i"」。这两件事不同,
  // 而后者才是互操作的实际身份来源。
  //
  // 为什么这条重要:Itanium ABI(GCC/Clang)下 `typeid(int).name()` 是 "i",而
  // **MSVC 的 `type_info::name()` 返回的是未修饰的可读名** —— "int"、"struct Foo"
  // (修饰形式在另一个 `raw_name()` 上)。所以不是「两种修饰方案不同」,而是
  // **压根不是同一种命名方案**:FNV("i") vs FNV("int"),结果毫不相干。
  //
  // 这两套默认 id 都只保证各自 ABI 内一致,不保证彼此相等。跨平台/跨工具链传
  // 自定义类型仍必须用 `LMFLOW_DECLARE_TYPE_NAME` 显式钉稳定名。
#ifdef _MSC_VER
  assert(std::strcmp(typeid(int).name(), "int") == 0);
  assert(std::strcmp(typeid(double).name(), "double") == 0);
  assert(lmflow::TypeId<int>() == 3143511548502526014ULL);
  assert(lmflow::TypeId<double>() == 11567507311810436776ULL);
#else
  assert(std::strcmp(typeid(int).name(), "i") == 0);
  assert(std::strcmp(typeid(double).name(), "d") == 0);
  assert(lmflow::TypeId<int>() == 12638195996648667684ULL);
  assert(lmflow::TypeId<double>() == 12638183902020757363ULL);
#endif
}
}  // namespace

int main() {
  // 1) 构造抛异常:create 返回 nullptr,绝不让异常穿越 extern "C"。
  {
    const LMFlowKernelVTable* vt = lmflow::KernelAdapter<ThrowingCtorKernel>::vtable();
    void* self = vt->create(nullptr);
    assert(self == nullptr && "create must return nullptr when the constructor throws");
  }

  // 2) create 失败后 self==nullptr:open/process/close 必须安全返回错误(不 null 解引用)。
  {
    const LMFlowKernelVTable* vt = lmflow::KernelAdapter<OkKernel>::vtable();
    assert(vt->open(nullptr, nullptr) == LMFLOW_ERR_KERNEL);
    assert(vt->process(nullptr, nullptr) == LMFLOW_ERR_KERNEL);
    assert(vt->close(nullptr, nullptr) == LMFLOW_ERR_KERNEL);
  }

  // 3) Process 抛异常:process 转成 LMFLOW_ERR_KERNEL。
  {
    const LMFlowKernelVTable* vt = lmflow::KernelAdapter<ThrowingProcessKernel>::vtable();
    void* self = vt->create(nullptr);
    assert(self != nullptr);
    assert(vt->process(self, nullptr) == LMFLOW_ERR_KERNEL);
    vt->destroy(self);
  }

  // 4) 正常算子:create 非空,process 返回 LMFLOW_OK。
  {
    const LMFlowKernelVTable* vt = lmflow::KernelAdapter<OkKernel>::vtable();
    void* self = vt->create(nullptr);
    assert(self != nullptr);
    assert(vt->process(self, nullptr) == LMFLOW_OK);
    vt->destroy(self);
  }

  // 5) GetContract 抛异常:必须记录到 Contract,不能静默继续建图。
  {
    g_contract_error[0] = '\0';
    const LMFlowKernelVTable* vt = lmflow::KernelAdapter<ThrowingContractKernel>::vtable();
    vt->get_contract(nullptr, kFakeContract);
    assert(std::strstr(g_contract_error, "contract boom") != nullptr);
  }

  test_type_id();

  std::printf("all flow.hpp unit tests passed\n");
  return 0;
}
