/* Windows replacement for libhevc common/ithread.c (pthreads). */
#include <windows.h>
#include <process.h>
#include <stdlib.h>
#include <string.h>

#include "ihevc_typedefs.h"
#include "ithread.h"

typedef struct {
    HANDLE handle;
} MkffThread;

typedef struct {
    CRITICAL_SECTION cs;
} MkffMutex;

typedef struct {
    HANDLE sem;
} MkffSem;

typedef struct {
    CONDITION_VARIABLE cv;
} MkffCond;

UWORD32 ithread_get_handle_size(void) { return (UWORD32)sizeof(MkffThread); }
UWORD32 ithread_get_mutex_lock_size(void) { return (UWORD32)sizeof(MkffMutex); }
WORD32 ithread_get_mutex_struct_size(void) { return (WORD32)sizeof(MkffMutex); }
UWORD32 ithread_get_sem_struct_size(void) { return (UWORD32)sizeof(MkffSem); }
WORD32 ithread_get_cond_struct_size(void) { return (WORD32)sizeof(MkffCond); }

typedef struct {
    void *(*start)(void *);
    void *arg;
} MkffThreadArgs;

static unsigned __stdcall thread_trampoline(void *p) {
    MkffThreadArgs *a = (MkffThreadArgs *)p;
    void *(*start)(void *) = a->start;
    void *arg = a->arg;
    free(a);
    start(arg);
    return 0;
}

WORD32 ithread_create(void *thread_handle, void *attribute, void *strt, void *argument) {
    (void)attribute;
    MkffThread *t = (MkffThread *)thread_handle;
    MkffThreadArgs *a = (MkffThreadArgs *)malloc(sizeof(*a));
    if (!a) {
        return -1;
    }
    a->start = (void *(*)(void *))strt;
    a->arg = argument;
    uintptr_t h = _beginthreadex(NULL, 0, thread_trampoline, a, 0, NULL);
    if (!h) {
        free(a);
        return -1;
    }
    t->handle = (HANDLE)h;
    return 0;
}

void ithread_exit(void *val_ptr) {
    (void)val_ptr;
    _endthreadex(0);
}

WORD32 ithread_join(void *thread_id, void **val_ptr) {
    (void)val_ptr;
    MkffThread *t = (MkffThread *)thread_id;
    WaitForSingleObject(t->handle, INFINITE);
    CloseHandle(t->handle);
    t->handle = NULL;
    return 0;
}

WORD32 ithread_mutex_init(void *mutex) {
    InitializeCriticalSection(&((MkffMutex *)mutex)->cs);
    return 0;
}

WORD32 ithread_mutex_destroy(void *mutex) {
    DeleteCriticalSection(&((MkffMutex *)mutex)->cs);
    return 0;
}

WORD32 ithread_mutex_lock(void *mutex) {
    EnterCriticalSection(&((MkffMutex *)mutex)->cs);
    return 0;
}

WORD32 ithread_mutex_unlock(void *mutex) {
    LeaveCriticalSection(&((MkffMutex *)mutex)->cs);
    return 0;
}

void ithread_yield(void) { SwitchToThread(); }
void ithread_sleep(UWORD32 u4_time) { Sleep(u4_time * 1000u); }
void ithread_msleep(UWORD32 u4_time_ms) { Sleep(u4_time_ms); }
void ithread_usleep(UWORD32 u4_time_us) {
    if (u4_time_us < 1000) {
        Sleep(1);
    } else {
        Sleep(u4_time_us / 1000u);
    }
}

WORD32 ithread_sem_init(void *sem, WORD32 pshared, UWORD32 value) {
    (void)pshared;
    MkffSem *s = (MkffSem *)sem;
    s->sem = CreateSemaphoreW(NULL, (LONG)value, 0x7fffffff, NULL);
    return s->sem ? 0 : -1;
}

WORD32 ithread_sem_post(void *sem) {
    return ReleaseSemaphore(((MkffSem *)sem)->sem, 1, NULL) ? 0 : -1;
}

WORD32 ithread_sem_wait(void *sem) {
    return WaitForSingleObject(((MkffSem *)sem)->sem, INFINITE) == WAIT_OBJECT_0 ? 0 : -1;
}

WORD32 ithread_sem_destroy(void *sem) {
    CloseHandle(((MkffSem *)sem)->sem);
    return 0;
}

WORD32 ithread_set_affinity(WORD32 core_id) {
    (void)core_id;
    return 0;
}

WORD32 ithread_cond_init(void *cond) {
    InitializeConditionVariable(&((MkffCond *)cond)->cv);
    return 0;
}

WORD32 ithread_cond_destroy(void *cond) {
    (void)cond;
    return 0;
}

WORD32 ithread_cond_wait(void *cond, void *mutex) {
    SleepConditionVariableCS(&((MkffCond *)cond)->cv, &((MkffMutex *)mutex)->cs, INFINITE);
    return 0;
}

WORD32 ithread_cond_signal(void *cond) {
    WakeConditionVariable(&((MkffCond *)cond)->cv);
    return 0;
}
