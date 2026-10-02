use inflow_core::Environment;

#[test]
fn production_is_the_default_environment() {
    assert_eq!(Environment::default(), Environment::Production);
    assert_eq!(
        Environment::default().api_base_url(),
        "https://api.inflowpay.ai"
    );
}

#[test]
fn sandbox_uses_the_sandbox_api() {
    assert_eq!(
        Environment::Sandbox.api_base_url(),
        "https://sandbox.inflowpay.ai"
    );
}
