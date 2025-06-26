use lazy_static::lazy_static;
use std::env;

macro_rules! env_var {
    ($name:expr) => {
        env::var($name).expect(&format!("{} must be set", $name))
    };
}

macro_rules! env_var_or {
    ($name:expr, $default:expr) => {
        env::var($name).unwrap_or($default.into())
    };
}

lazy_static! {
    /// The port where the worker will be listening
    pub static ref PORT: u16 = env_var_or!("PORT", "6969")
        .parse::<u16>().expect("PORT must be a number");

    /// The host where the worker will be listening
    pub static ref HOST: String = env_var_or!("HOST", "0.0.0.0");

    pub static ref CARGO_PKG_NAME: String = env_var_or!("CARGO_PKG_NAME", "nur_gateway");

    pub static ref POSTGRES_URL: String = env_var!("POSTGRES_URL");
}
