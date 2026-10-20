#ifndef SHARED_LIB_INTERFACE_H
#define SHARED_LIB_INTERFACE_H

#include "stdbool.h"
#include "connection.h"

#ifdef __cplusplus
extern "C"
{	
#endif
	
#if (defined(WIN32) || defined(WIN64) || defined(_MSC_VER) || defined(_WIN32))
#define MULET_EXPORT(type)   __declspec(dllexport) type __stdcall
#elif (defined(__GNUC__) || defined(__SUNPRO_CC) || defined (__SUNPRO_C))
#define MULET_EXPORT(type)   __attribute__((visibility("default"))) type
#else
#define MULET_EXPORT(type) type
#endif 
	 
/*
 !!! mulet shared library export interface，shared library must implement all interface。!!!
 */

#define SHARED_LIB_VERSION 					"1.0.0"
	
/** shared library naming rules：lib + mulet module name（'-' change to '_'）+ suffix（such as:.so）*/

/** library export interface*/

/**
 * @brief mulet main entry
 * @param argc
 * @param argv
 * @return 0: normal starup; not 0 starup with exception
 */
MULET_EXPORT(int) mulet_start(int argc, char *argv[]);

/**
 * @brief set pointer that to operate connection pool for retrieve and enlist connection
 * @param connection_legion pointer to connection_caller_t
 * @return 0: success, fail not 0
 */
MULET_EXPORT(int) mulet_set_connection(const connection_legion_t *connection_legion);

/**
 * @brief A number that represents this kind of mulets in manager_internal_t.
 * @return the crc64 checksum for this mulet.
 */
MULET_EXPORT(char*) mulet_checksum(void);

/**
 * @brief process after shared library running
 * @param raw server connection_t for network request
 * @return 0: keep worker thread, the mulet must hold the worker;
 *         not 0: exit worker thread
 */
MULET_EXPORT(int) mulet_process(connection_t *raw);

/**
 * @brief get mulet activated status
 * @return true: activated, false: unactivated
 */
MULET_EXPORT(bool) mulet_is_activated(void);

/**
 * @brief close mulet, notify mulet cleaning resource to stop running
 */
MULET_EXPORT(void) mulet_stop(void);
	
#ifdef __cplusplus
}
#endif
#endif
