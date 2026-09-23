// TinyUSB glue for the Switch controller: builds the device/configuration
// descriptors from the Rust-side config and implements the HID callbacks.

#include "switch_hid.h"

#include <string.h>

#include "esp_log.h"
#include "tinyusb.h"

static const char *TAG = "switch_hid";

#define EP_OUT 0x02
#define EP_IN 0x81
#define EP_SIZE 64
#define CONFIG_TOTAL_LEN (TUD_CONFIG_DESC_LEN + TUD_HID_INOUT_DESC_LEN)

static const switch_hid_config_t *s_config;
static tusb_desc_device_t s_device;
static uint8_t s_configuration[CONFIG_TOTAL_LEN];
static const char *s_strings[4];
static const char s_language[] = {0x09, 0x04}; // English (US)

esp_err_t switch_hid_init(const switch_hid_config_t *config)
{
    s_config = config;
    s_device = (tusb_desc_device_t){
        .bLength = sizeof(tusb_desc_device_t),
        .bDescriptorType = TUSB_DESC_DEVICE,
        .bcdUSB = 0x0200,
        .bDeviceClass = 0x00, // per interface
        .bDeviceSubClass = 0x00,
        .bDeviceProtocol = 0x00,
        .bMaxPacketSize0 = CFG_TUD_ENDPOINT0_SIZE,
        .idVendor = config->vendor_id,
        .idProduct = config->product_id,
        .bcdDevice = config->bcd_device,
        .iManufacturer = 1,
        .iProduct = 2,
        .iSerialNumber = 3,
        .bNumConfigurations = 1,
    };

    const uint8_t configuration[] = {
        TUD_CONFIG_DESCRIPTOR(1, 1, 0, CONFIG_TOTAL_LEN, 0x80, 500),
        TUD_HID_INOUT_DESCRIPTOR(0, 0, HID_ITF_PROTOCOL_NONE, config->report_descriptor_len,
                                 EP_OUT, EP_IN, EP_SIZE, config->poll_interval_ms),
    };
    _Static_assert(sizeof(configuration) == CONFIG_TOTAL_LEN, "configuration length");
    memcpy(s_configuration, configuration, sizeof(configuration));

    s_strings[0] = s_language;
    s_strings[1] = config->manufacturer;
    s_strings[2] = config->product;
    s_strings[3] = config->serial;

    const tinyusb_config_t tusb_cfg = {
        .device_descriptor = &s_device,
        .string_descriptor = s_strings,
        .string_descriptor_count = sizeof(s_strings) / sizeof(s_strings[0]),
        .external_phy = false,
        .configuration_descriptor = s_configuration,
        .self_powered = false, // powered by the Switch dock/port
    };

    esp_err_t err = tinyusb_driver_install(&tusb_cfg);
    if (err == ESP_OK) {
        ESP_LOGI(TAG, "USB HID %04x:%04x \"%s\" ready", config->vendor_id, config->product_id,
                 config->product);
    }
    return err;
}

bool switch_hid_mounted(void)
{
    return tud_mounted();
}

bool switch_hid_send(const uint8_t *report, size_t len)
{
    if (!tud_mounted()) {
        return false;
    }
    if (tud_suspended()) {
        tud_remote_wakeup();
        return false;
    }
    if (!tud_hid_ready()) {
        return false;
    }
    return tud_hid_report(0, report, (uint16_t)len);
}

// ---- TinyUSB HID callbacks ----

uint8_t const *tud_hid_descriptor_report_cb(uint8_t instance)
{
    (void)instance;
    return s_config->report_descriptor;
}

uint16_t tud_hid_get_report_cb(uint8_t instance, uint8_t report_id, hid_report_type_t report_type,
                               uint8_t *buffer, uint16_t reqlen)
{
    (void)instance;
    (void)report_id;
    (void)report_type;
    (void)buffer;
    (void)reqlen;
    return 0;
}

void tud_hid_set_report_cb(uint8_t instance, uint8_t report_id, hid_report_type_t report_type,
                           uint8_t const *buffer, uint16_t bufsize)
{
    // The Switch sends 8-byte vendor output reports to wired pads; nothing to do.
    (void)instance;
    (void)report_id;
    (void)report_type;
    (void)buffer;
    (void)bufsize;
}
