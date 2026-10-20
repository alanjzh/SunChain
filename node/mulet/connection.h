#ifndef CONNECTION_H_
#define CONNECTION_H_

#include <stdint.h>
#include <stdbool.h>

/**
 * the define of connection_t from connection_t pool that design by mudem, 
 */
struct connection_t;
/**
 * Make the subclasses can offer reading and writing objects with greater
 * granularity.  Apart from inheritance, decorator design pattern can be
 * applied to extend the functionality of this class dynamically.
 */
//typedef void (* connection_fp)(router_requet_t *request);

/**
 * <code>true</code> if this is a client side connection.
 */
typedef bool (*is_client_fp)(struct connection_t* self);

/**
 * Test if this connection is alive.
 */
typedef bool (*is_alive_fp)(struct connection_t* self);

/**
 * About to begin a session.
 */
typedef void (*begin_fp)(struct connection_t* self);

/**
 * About to end a session.
 */
typedef void (*end_fp)(struct connection_t* self);

/**
 *	Get uuid of connection
*/
typedef char* (*get_uuid_fp)(struct connection_t* self);

/**
 * Close the socket channel and cancel the selection key.
 */
typedef void (*close_fp)(struct connection_t* self);

/**
 * Ask the underlying connection to read the data.
 */
typedef int (*read_fp)(struct connection_t* self, uint8_t* bytes, int start, int length);

/**
 * Place a number of bytes to the write queue.
 */
typedef int (*write_fp)(struct connection_t* self, uint8_t* bytes, int start, int length);

/**
 * This represents a connection_t to this workstation from a remote host.
 * the define of connection_t from connection_t pool that design by mudem.
 */
typedef struct connection_t
{
    /**
     * @warning: internal must place at the first member of this struct.
     * we can use it to cast this struct to connection_internal_t
     */
    void* connection_internal;

    // function pointers for connection operations
    is_client_fp is_client;
    is_alive_fp is_alive;
    begin_fp begin;
    end_fp end;
    close_fp close;
    read_fp read;
    write_fp write;
    get_uuid_fp get_uuid;
} connection_t;

/**
 * is a client or server connection
 */
bool connection_is_client(connection_t* self);

/**
* Test if this connection is alive.
*/
bool connection_is_alive(connection_t* self);

/**
* About to begin a session.
*/
void connection_begin(connection_t* self);

/**
* About to end a session.
*/
void connection_end(connection_t* self);

/**
* Get the uuid for this connection.
*
* @return the uuid of this connection, or NULL if not set.
*/
char* connection_get_uuid(connection_t* self);

/**
* Close the socket channel and cancel the selection key.
*/
void connection_close(connection_t* self);

/**
* Ask the underlying connection to read the data.
*/
int connection_read(connection_t* self, uint8_t* bytes, int start, int length);

/**
* Place a number of bytes to the write queue.
*/
int connection_write(connection_t* self, uint8_t* bytes, int start, int length);

//connection pool interface

/*
 * @brief Get a connection by its mudem ID.
 *        This function retrieves a connection from the global connection pools based on the provided mudem ID.
 *        It will involve network discover to find suitable router, and send router request to the router.
 *
 * @param mudem_id The mudem ID of the connection to retrieve.
 *
 * @return A pointer to the connection_t structure if found, NULL otherwise.
 */
typedef connection_t* (*get_connection_fp)(char *mudem_id);

/**
 * @brief return back a connection to the pool. Connection will be destroyed if it's not alive.
 *			when pool is full, it will wait until pool is not full.
 *
 * @param connection The connection to be returned to the pool.
 *
 * @return 0 on success, non-zero on failure.
 */
typedef int (*put_connection_fp)(connection_t* connection);

/**
 * @brief global connection legion function pointers
 */
typedef struct connection_legion_t
{
    /** get connection from pool */
    get_connection_fp get_connection;
    /** return connection to pool */
    put_connection_fp put_connection;
} connection_legion_t;

#endif /** CONNECTION_H_ */
