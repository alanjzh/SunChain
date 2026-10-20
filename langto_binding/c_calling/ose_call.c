//cc -o c_calling/ose_call{,.c} -L target/debug -l langto_binding -l{pthread,dl,m}
// export LD_LIBRARY_PATH=${LD_LIBRARY_PATH}:/home/clouder/sun_dev/sunchain/langto_binding/target/debug

#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#include "../sunchain_langto_binding.h"



int call_ose_method(char* m, char* p)
{

    // char const * method = "rpc_methods";
    // char const * params = "{\"params\": []}";
    char const * result = NULL;
    OseCall_t call = {
        .method = { .ptr = (uint8_t *)m, .len = strlen(m) },
        .params = { .ptr = (uint8_t *)p, .len = strlen(p) },
        .result = { .ptr = (uint8_t *)result, .len = 0 }
    };
    uint64_t code = call_by_ose_use_box(&call);
    printf("Call res: %s\n", (call.result.ptr != NULL ? (char *)call.result.ptr : "NULL"));
    return (int)code;
}

// void mallocStruct(){
//     malloc(sizeof(OseCall_t));
// }

int
main (int argc, char const * const argv[])
{
    // call_ose_method("rpc_methods", "\"params\": [\"block_number\":123]");

    // slice_boxed_uint8_t res = call_by_ose_with_char_p("rpc_methods", "[]");
    // slice_boxed_uint8_t res = call_by_ose_with_char_p("system_chain", "[]");
    slice_boxed_uint8_t res = call_by_ose_with_char_p("chain_getHeader", "[]");
    // slice_boxed_uint8_t res = call_by_ose_with_char_p("state_getStorage", 
    //  "[\"0x26aa394eea5630e07c48ae0c9558cef7b99d880ec681799c0cf30e8886371da9de1e86a9a8c739864cf3cc5ec2bea59fd43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d\"]");
    printf("Call res: %s\n", (res.ptr != NULL ? (char *)res.ptr : "NULL"));
    release_str_box(res);

    // for(size_t i = 0; i < 10000 ; ++ i){
    //     slice_boxed_uint8_t res = call_by_ose_with_char_p("system_chain", "[]");
    //     printf("Call res: %s\n", (res.ptr != NULL ? (char *)res.ptr : "NULL"));
    //     release_str_box(res);
    // }

    return EXIT_SUCCESS;
}