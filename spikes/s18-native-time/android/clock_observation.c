// Diagnostic references bypass the virtual-clock interposer; they never advance application time.
#include "clock_observation.h"

JNIEXPORT jlongArray JNICALL Java_dev_frontcap_s18_Clocks_clockObservation(JNIEnv *env, jclass cls) {
  (void)cls;
  return diagnostic_clock_observation(env);
}
