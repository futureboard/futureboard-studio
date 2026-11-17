#ifndef MKFF_EXPORT_H
#define MKFF_EXPORT_H

#if defined(_WIN32)
#  if defined(MKFF_BUILDING_SHARED)
#    define MKFF_API __declspec(dllexport)
#  elif defined(MKFF_STATIC)
#    define MKFF_API
#  else
#    define MKFF_API __declspec(dllimport)
#  endif
#else
#  if defined(MKFF_BUILDING_SHARED)
#    define MKFF_API __attribute__((visibility("default")))
#  else
#    define MKFF_API
#  endif
#endif

#if defined(__cplusplus)
#  define MKFF_BEGIN_DECLS extern "C" {
#  define MKFF_END_DECLS }
#else
#  define MKFF_BEGIN_DECLS
#  define MKFF_END_DECLS
#endif

#endif /* MKFF_EXPORT_H */
