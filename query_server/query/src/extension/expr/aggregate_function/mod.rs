#[cfg(test)]
mod example;

use spi::query::function::FunctionMetadataManager;
use spi::Result;

pub fn register_udafs(_func_manager: &mut dyn FunctionMetadataManager) -> Result<()> {
    // extend function...
    // eg.
    //   example::register_udaf(func_manager)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::function::simple_func_manager::SimpleFunctionMetadataManager;

    use super::*;

    #[tokio::test]
    async fn test_example() {
        let mut func_manager = SimpleFunctionMetadataManager::default();

        let expect_udaf = example::register_udaf(&mut func_manager);

        assert!(expect_udaf.is_ok(), "register_udaf error.");

        let expect_udaf = expect_udaf.unwrap();

        let result_udaf = func_manager.udaf(&expect_udaf.name);

        assert!(result_udaf.is_ok(), "not get result from func manager.");

        assert_eq!(&expect_udaf, result_udaf.unwrap().as_ref());
    }
}
