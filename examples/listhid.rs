fn main() {
    let api = hidapi::HidApi::new().unwrap();
    for d in api.device_list() {
        println!("{:04x}:{:04x} page {:#06x} usage {:#04x}  {:?} / {:?}", d.vendor_id(), d.product_id(),
            d.usage_page(), d.usage(), d.manufacturer_string(), d.product_string());
    }
}
