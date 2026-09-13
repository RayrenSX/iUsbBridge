#include <windows.h>
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#ifdef interface
#undef interface
#endif

#pragma pack(push, 1)
typedef struct usb_device_descriptor { unsigned char bLength,bDescriptorType; unsigned short bcdUSB; unsigned char bDeviceClass,bDeviceSubClass,bDeviceProtocol,bMaxPacketSize0; unsigned short idVendor,idProduct,bcdDevice; unsigned char iManufacturer,iProduct,iSerialNumber,bNumConfigurations; } usb_device_descriptor;
typedef struct usb_endpoint_descriptor { unsigned char bLength,bDescriptorType,bEndpointAddress,bmAttributes; unsigned short wMaxPacketSize; unsigned char bInterval,bRefresh,bSynchAddress; unsigned char *extra; int extralen; } usb_endpoint_descriptor;
typedef struct usb_interface_descriptor { unsigned char bLength,bDescriptorType,bInterfaceNumber,bAlternateSetting,bNumEndpoints,bInterfaceClass,bInterfaceSubClass,bInterfaceProtocol,iInterface; usb_endpoint_descriptor *endpoint; unsigned char *extra; int extralen; } usb_interface_descriptor;
typedef struct usb_interface { usb_interface_descriptor *altsetting; int num_altsetting; } usb_interface;
typedef struct usb_config_descriptor { unsigned char bLength,bDescriptorType; unsigned short wTotalLength; unsigned char bNumInterfaces,bConfigurationValue,iConfiguration,bmAttributes,MaxPower; usb_interface *interface; unsigned char *extra; int extralen; } usb_config_descriptor;
typedef struct usb_device { struct usb_device *next,*prev; char filename[512]; void *bus; usb_device_descriptor descriptor; usb_config_descriptor *config; void *dev; unsigned char devnum,num_children; struct usb_device **children; } usb_device;
typedef struct usb_bus { struct usb_bus *next,*prev; char dirname[512]; usb_device *devices; unsigned long location; usb_device *root_dev; } usb_bus;
typedef struct usb_dev_handle usb_dev_handle;
#pragma pack(pop)

typedef char usb_device_abi_packing_mismatch[
    offsetof(usb_device, config) ==
            3 * sizeof(void *) + 512 + sizeof(usb_device_descriptor)
        ? 1
        : -1];
typedef struct { HMODULE dll; void (*init)(void); int (*find_busses)(void),(*find_devices)(void); usb_bus *(*get_busses)(void); usb_dev_handle *(*open)(usb_device*); int (*close)(usb_dev_handle*); int (*claim_interface)(usb_dev_handle*,int); int (*clear_halt)(usb_dev_handle*,unsigned int); int (*bulk_read)(usb_dev_handle*,int,char*,int,int); int (*bulk_write)(usb_dev_handle*,int,char*,int,int); int (*get_string_simple)(usb_dev_handle*,int,char*,size_t); } api_t;
static api_t a;
static int serial_equal(const char* left,const char* right){
 if(!left||!right)return 0;
 for(;;){
  while(*left=='-')left++;
  while(*right=='-')right++;
  unsigned char l=(unsigned char)*left,r=(unsigned char)*right;
  if(l>='a'&&l<='z')l=(unsigned char)(l-'a'+'A');
  if(r>='a'&&r<='z')r=(unsigned char)(r-'a'+'A');
  if(l!=r)return 0;
  if(!l)return 1;
  left++;right++;
 }
}
static int api_ready(void){ return a.dll && a.init && a.find_busses && a.find_devices && a.get_busses && a.open && a.close && a.claim_interface && a.clear_halt && a.bulk_read && a.bulk_write && a.get_string_simple; }
static int load_api(void){
 if(api_ready())return 1;
 if(a.dll){ FreeLibrary(a.dll); ZeroMemory(&a,sizeof(a)); }
 a.dll=LoadLibraryA("libusb0.dll");
 if(!a.dll){
  char module[MAX_PATH]={0};
  DWORD n=GetModuleFileNameA(NULL,module,MAX_PATH);
  if(n>0 && n<MAX_PATH){
   char *slash=strrchr(module,'\\');
   if(slash){
    strcpy(slash+1,"libusb0.dll");
    a.dll=LoadLibraryA(module);
   }
   if(!a.dll && slash){
    char *parent=strrchr(module,'\\');
    if(parent){ strcpy(parent+1,"libusb0.dll"); a.dll=LoadLibraryA(module); }
   }
  }
 }
 if(!a.dll)return 0;
#define L(x) a.x=(void*)GetProcAddress(a.dll,"usb_" #x); if(!a.x){ FreeLibrary(a.dll); ZeroMemory(&a,sizeof(a)); return 0; }
 L(init);L(find_busses);L(find_devices);L(get_busses);L(open);L(close);L(claim_interface);L(clear_halt);L(bulk_read);L(bulk_write);L(get_string_simple); return 1; }
__declspec(dllexport) void* im_libusb0_open(const char* serial, int *in_ep, int *out_ep)
{
    if (!load_api()) {
        fprintf(stderr, "legacy mux: libusb0 API unavailable\n");
        return 0;
    }

    a.init();
    a.find_busses();
    a.find_devices();

    for (usb_bus *bus = a.get_busses(); bus; bus = bus->next) {
        for (usb_device *device = bus->devices; device; device = device->next) {
            if (device->descriptor.idVendor != 0x05ac) {
                continue;
            }

            char device_serial[128] = {0};
            usb_dev_handle *handle = a.open(device);
            if (!handle) {
                fprintf(stderr, "legacy mux: Apple device open failed\n");
                continue;
            }

            if (device->descriptor.iSerialNumber > 0) {
                a.get_string_simple(handle, device->descriptor.iSerialNumber,
                                    device_serial, sizeof(device_serial));
            }
            fprintf(stderr, "legacy mux: Apple serial=%s configs=%u\n",
                    device_serial, device->descriptor.bNumConfigurations);

            if (serial && !serial_equal(serial, device_serial)) {
                a.close(handle);
                continue;
            }

            *in_ep = 0;
            *out_ep = 0;
            if (!device->config) {
                a.close(handle);
                continue;
            }
            for (int c = 0; c < device->descriptor.bNumConfigurations; c++) {
                usb_config_descriptor *config = &device->config[c];
                fprintf(stderr, "legacy mux: config=%u interfaces=%u\n",
                        config->bConfigurationValue, config->bNumInterfaces);

                /* The projection process already selected QuickTime config 5. */
                if (config->bConfigurationValue != 5) {
                    continue;
                }

                for (int i = 0; i < config->bNumInterfaces; i++) {
                    usb_interface_descriptor *interface_desc =
                        config->interface[i].altsetting;
                    if (!interface_desc ||
                        interface_desc->bInterfaceClass != 0xff ||
                        interface_desc->bInterfaceSubClass != 0xfe) {
                        continue;
                    }

                    for (int e = 0; e < interface_desc->bNumEndpoints; e++) {
                        unsigned char address =
                            interface_desc->endpoint[e].bEndpointAddress;
                        if (address & 0x80) {
                            *in_ep = address;
                        } else {
                            *out_ep = address;
                        }
                    }

                    int claim = (*in_ep && *out_ep)
                                    ? a.claim_interface(handle,
                                                        interface_desc->bInterfaceNumber)
                                    : -1;
                    fprintf(stderr,
                            "legacy mux: interface=%u in=0x%02x out=0x%02x claim=%d\n",
                            interface_desc->bInterfaceNumber, *in_ep, *out_ep, claim);
                    if (claim == 0) {
                        a.clear_halt(handle, *in_ep);
                        a.clear_halt(handle, *out_ep);
                        return handle;
                    }
                }
            }

            a.close(handle);
        }
    }

    return 0;
}

__declspec(dllexport) int im_libusb0_read(void *handle, int ep, char *buffer, int length, int timeout)
{
    if (!api_ready() || !handle) {
        return -1;
    }
    return a.bulk_read((usb_dev_handle *)handle, ep, buffer, length, timeout);
}

__declspec(dllexport) int im_libusb0_write(void *handle, int ep, char *buffer, int length, int timeout)
{
    if (!api_ready() || !handle) {
        return -1;
    }
    return a.bulk_write((usb_dev_handle *)handle, ep, buffer, length, timeout);
}

__declspec(dllexport) void im_libusb0_close(void *handle)
{
    if (handle && api_ready()) {
        a.close((usb_dev_handle *)handle);
    }
}
