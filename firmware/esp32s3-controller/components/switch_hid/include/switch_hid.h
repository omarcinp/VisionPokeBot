// USB side of the controller: TinyUSB configured as a single HID gamepad.
// The descriptors come from Rust (pokebot_remote::hid) so there is one
// source of truth for the device identity and report layout.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "esp_err.h"

typedef struct {
    uint16_t vendor_id;
    uint16_t product_id;
    uint16_t bcd_device;
    const char *manufacturer;
    const char *product;
    const char *serial;
    const uint8_t *report_descriptor;
    uint16_t report_descriptor_len;
    uint8_t poll_interval_ms;
} switch_hid_config_t;

// Installs TinyUSB on the native USB port. `config` and the strings and
// descriptor it points to must outlive the program.
esp_err_t switch_hid_init(const switch_hid_config_t *config);

// True while a host (the Switch) has the device configured.
bool switch_hid_mounted(void);

// Queues one input report. Returns false if the endpoint is still busy with
// the previous one; the caller retries on its next tick.
bool switch_hid_send(const uint8_t *report, size_t len);
