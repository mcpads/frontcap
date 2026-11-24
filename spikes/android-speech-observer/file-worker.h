// Included only by the optional platform file-preparation experiment.
#include <sys/random.h>
#include <limits>
#ifdef FRONTCAP_SPEECH_CONTROL
#include "cursor.h"
#endif

static jclass worker_class;
static jmethodID worker_prepare;
#ifdef FRONTCAP_SPEECH_CONTROL
static jmethodID worker_stop;
#endif
static std::string worker_identity;
static bool worker_requested;
static unsigned worker_requests;
static bool worker_dispatching;
static bool observed_complete;
static constexpr unsigned worker_ordinal = std::numeric_limits<unsigned>::max();

// Resolve through the already-authorized debugger interface. JNI name lookup is filtered
// by Android's hidden-API policy; no host policy or consumer SDK configuration is changed.
static jfieldID framework_field(jvmtiEnv* ti, jclass type, const char* wanted, const char* descriptor) {
    jint count = 0;
    jfieldID* fields = nullptr;
    if (ti->GetClassFields(type, &count, &fields) != JVMTI_ERROR_NONE) return nullptr;
    jfieldID result = nullptr;
    for (jint i = 0; i < count; ++i) {
        char *name = nullptr, *signature = nullptr;
        if (ti->GetFieldName(type, fields[i], &name, &signature, nullptr) != JVMTI_ERROR_NONE) continue;
        if (!strcmp(name, wanted) && !strcmp(signature, descriptor)) result = fields[i];
        ti->Deallocate(reinterpret_cast<unsigned char*>(name));
        ti->Deallocate(reinterpret_cast<unsigned char*>(signature));
    }
    ti->Deallocate(reinterpret_cast<unsigned char*>(fields));
    return result;
}

static bool load_worker(JNIEnv* env, const char* directory) {
    unsigned char random[16];
    if (getrandom(random, sizeof(random), 0) != sizeof(random)) return false;
    for (unsigned char byte : random) {
        char hex[3];
        snprintf(hex, sizeof(hex), "%02x", byte);
        worker_identity += hex;
    }
    if (env->PushLocalFrame(16) != JNI_OK) return false;
    bool ok = [&]() {
        jclass loaders = env->FindClass("java/lang/ClassLoader");
        if (!loaders) return false;
        auto system = env->GetStaticMethodID(loaders, "getSystemClassLoader", "()Ljava/lang/ClassLoader;");
        if (!system) return false;
        jobject parent = env->CallStaticObjectMethod(loaders, system);
        if (env->ExceptionCheck() || !parent) return false;
        auto load = env->GetMethodID(loaders, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;");
        if (!load) return false;
        jclass dex = env->FindClass("dalvik/system/DexClassLoader");
        if (!dex) return false;
        auto constructor = env->GetMethodID(dex, "<init>",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/ClassLoader;)V");
        if (!constructor) return false;
        jstring path = env->NewStringUTF((std::string(directory) + "/speech-worker.jar").c_str());
        if (!path) return false;
        jobject loader = env->NewObject(dex, constructor, path, nullptr, nullptr, parent);
        if (env->ExceptionCheck() || !loader) return false;
        jstring name = env->NewStringUTF("dev.frontcap.speech.SpeechFileWorker");
        if (!name) return false;
        jobject type = env->CallObjectMethod(loader, load, name);
        if (env->ExceptionCheck() || !type) return false;
        worker_prepare = env->GetStaticMethodID(static_cast<jclass>(type), "prepare",
            "(Landroid/content/Context;Landroid/speech/tts/TextToSpeech;Ljava/lang/String;"
            "Landroid/os/Bundle;Landroid/os/Bundle;Ljava/lang/String;Ljava/lang/String;"
            "Landroid/speech/tts/UtteranceProgressListener;Ljava/lang/String;)Z");
        if (!worker_prepare) return false;
#ifdef FRONTCAP_SPEECH_CONTROL
        if (!register_cursor(env, static_cast<jclass>(type))) return false;
        worker_stop = env->GetStaticMethodID(static_cast<jclass>(type), "stop", "()Z");
        if (!worker_stop) return false;
#endif
        worker_class = static_cast<jclass>(env->NewGlobalRef(type));
        return worker_class != nullptr;
    }();
    if (env->ExceptionCheck()) env->ExceptionClear();
    env->PopLocalFrame(nullptr);
    return ok;
}

// Called without the observer mutex: these Java calls can prepare classes or reenter breakpoints.
static bool prepare_worker(jvmtiEnv* ti, JNIEnv* env, jthread thread,
                           jint text_slot, jint params_slot, jint identity_slot,
                           const std::string& token, const char** stage) {
#ifndef FRONTCAP_SPEECH_CONTROL
    (void)identity_slot;
#endif
    *stage = "local_frame";
    if (env->PushLocalFrame(16) != JNI_OK) {
        if (env->ExceptionCheck()) env->ExceptionClear();
        return false;
    }
    bool ok = [&]() {
        jobject source = nullptr, text = nullptr, request = nullptr;
        *stage = "argument_slots";
        if (text_slot < 0 || params_slot < 0) return false;
        *stage = "receiver";
        if (ti->GetLocalInstance(thread, 0, &source) != JVMTI_ERROR_NONE || !source) return false;
        *stage = "text";
        if (ti->GetLocalObject(thread, 0, text_slot, &text) != JVMTI_ERROR_NONE || !text ||
            !env->IsInstanceOf(text, string_class)) return false;
        *stage = "request";
        if (ti->GetLocalObject(thread, 0, params_slot, &request) != JVMTI_ERROR_NONE) return false;
        *stage = "source_type";
        jclass type = env->FindClass("android/speech/tts/TextToSpeech");
        if (!type) return false;
        *stage = "context_field";
        auto context_field = framework_field(ti, type, "mContext", "Landroid/content/Context;");
        if (!context_field) return false;
        *stage = "params_field";
        auto params_field = framework_field(ti, type, "mParams", "Landroid/os/Bundle;");
        if (!params_field) return false;
        *stage = "engine_field";
        auto engine_field = framework_field(ti, type, "mCurrentEngine", "Ljava/lang/String;");
        if (!engine_field) return false;
        *stage = "field_values";
        jobject context = env->GetObjectField(source, context_field);
        jobject defaults = env->GetObjectField(source, params_field);
        jobject engine = env->GetObjectField(source, engine_field);
        if (env->ExceptionCheck() || !context || !defaults || !engine) return false;
        *stage = "identity";
        jstring id = env->NewStringUTF(token.c_str());
        if (!id) return false;
        jobject listener = nullptr, consumer_id = nullptr;
#ifdef FRONTCAP_SPEECH_CONTROL
        *stage = "consumer_listener";
        speech_listener_field = framework_field(ti, type, "mUtteranceProgressListener",
            "Landroid/speech/tts/UtteranceProgressListener;");
        if (!speech_listener_field) return false;
        listener = env->GetObjectField(source, speech_listener_field);
        if (env->ExceptionCheck() || !listener) return false;
        *stage = "consumer_identity";
        if (identity_slot < 0 ||
            ti->GetLocalObject(thread, 0, identity_slot, &consumer_id) != JVMTI_ERROR_NONE ||
            !consumer_id || !env->IsInstanceOf(consumer_id, string_class)) return false;
        if (!controlled_source) controlled_source = env->NewWeakGlobalRef(source);
        if (!controlled_source) return false;
#endif
        *stage = "java_prepare";
        bool accepted = env->CallStaticBooleanMethod(worker_class, worker_prepare, context, source, text,
                                 defaults, request, engine, id, listener, consumer_id);
        if (env->ExceptionCheck() || !accepted) return false;
        *stage = "submitted";
        return true;
    }();
    if (env->ExceptionCheck()) env->ExceptionClear();
    env->PopLocalFrame(nullptr);
    return ok;
}
