//! 测试共享设施。依赖进程级环境变量(`ZNAIDE_DATA_DIR`)的测试必须先拿这把锁 ——
//! 同一个测试二进制里各测试是并行线程、env 是共享的,各拿各的私有锁等于没锁:
//! 一个用例会把另一个用例的数据目录换掉(表现为"单独跑绿、一起跑红")。
pub static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
