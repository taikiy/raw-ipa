use ipa_macros::Step;

use crate::{
    error::Error,
    ff::{ArrayAccess, CustomArray, Expand, Field},
    protocol::{basics::SecureMul, context::Context, step::BitOpStep, RecordId},
    secret_sharing::{replicated::semi_honest::AdditiveShare, WeakSharedValue},
};

#[derive(Step)]
pub(crate) enum Step {
    SaturatedAddition,
    MultiplyWithCarry,
}

///non-saturated unsigned integer addition
/// adds y to x, Output has same length as x (carries and indices of y too large for x are ignored)
/// # Errors
/// propagates errors from multiply
pub async fn integer_add<C, XS, YS>(
    ctx: C,
    record_id: RecordId,
    x: &AdditiveShare<XS>,
    y: &AdditiveShare<XS>,
    // y: &AdditiveShare<YS>,
) -> Result<AdditiveShare<XS>, Error>
where
    C: Context,
    for<'a> &'a AdditiveShare<XS>: IntoIterator<Item = AdditiveShare<XS::Element>>,
    // YS: WeakSharedValue + CustomArray<Element = XS::Element>,
    XS: WeakSharedValue + CustomArray +Field,
    XS::Element: Field,
{
    let mut carry = AdditiveShare::<XS::Element>::ZERO;
    addition_circuit(ctx, record_id, x, y, &mut carry).await
    // x.multiply(y, ctx.narrow(&Step::MultiplyWithCarry), RecordId(0)).await
}

///saturated unsigned integer addition
/// adds y to x, Output has same length as x (carries and indices of y too large for x are ignored)
/// result is set to all one array (saturated, when carry of `x[x::BITS-1] + y[x::BITS-1] + Carry[x::BITS-2]` is 1)
/// ideally it would be all one array when any of higher bits of y is non-zero (not implemented since we dont seem to need it)
/// # Errors
/// propagates errors from multiply
pub async fn integer_sat_add<C, XS, YS>(
    ctx: C,
    record_id: RecordId,
    x: &AdditiveShare<XS>,
    y: &AdditiveShare<YS>,
) -> Result<AdditiveShare<XS>, Error>
where
    C: Context,
    for<'a> &'a AdditiveShare<XS>: IntoIterator<Item = AdditiveShare<XS::Element>>,
    XS: CustomArray + Field,
    YS: WeakSharedValue + CustomArray<Element = XS::Element>,
    XS::Element: Field,
{
    let mut carry = AdditiveShare::<XS::Element>::ZERO;
    let result = addition_circuit(
        ctx.narrow(&Step::SaturatedAddition),
        record_id,
        x,
        y,
        &mut carry,
    )
    .await?;

    //if carry==1 {all 1 array, i.e. Array[carry]} else {result}:
    //compute carry*Array[carry]+(1-carry)*result = result+carry(Array[carry]-result)
    let carry_array = AdditiveShare::<XS>::expand(&carry);
    let sat = result.clone()
        + carry_array
            .multiply(
                &(carry_array.clone() - result),
                ctx.narrow(&Step::MultiplyWithCarry),
                record_id,
            )
            .await?;
    Ok(sat)
}

///addition using bit adder
/// adds y to x, Output has same length as x (carries and indices of y too large for x are ignored)
///implementing `https://encrypto.de/papers/KSS09.pdf` from Section 3.1
///for all i: output[i] = x[i] + (c[i-1] + y[i])
/// # Errors
/// propagates errors from multiply
async fn addition_circuit<C, XS, YS>(
    ctx: C,
    record_id: RecordId,
    x: &AdditiveShare<XS>,
    y: &AdditiveShare<YS>,
    carry: &mut AdditiveShare<XS::Element>,
) -> Result<AdditiveShare<XS>, Error>
where
    C: Context,
    for<'a> &'a AdditiveShare<XS>: IntoIterator<Item = AdditiveShare<XS::Element>>,
    XS: WeakSharedValue + CustomArray,
    YS: WeakSharedValue + CustomArray<Element = XS::Element>,
    XS::Element: Field,
{
    let mut result = AdditiveShare::<XS>::ZERO;
    // let i=0;
    for (i, v) in x.into_iter().enumerate() {
        result.set(
            i.clone(),
            bit_adder(
                ctx.narrow(&BitOpStep::from(i.clone())),
                record_id,
                // &v,
                &x.get(i.clone()).unwrap(),
                &y.get(i).unwrap(),
                // y.get(i).as_ref(),
                // &y.get(i).unwrap(),
                carry,
            )
            .await?,
        );
    }

    Ok(result)
}

///bit adder
///implementing `https://encrypto.de/papers/KSS09.pdf` from Section 3.1
///output = x + (c + y)
///update carry to carry = ( x + carry)(y + carry) + carry
/// # Errors
/// propagates errors from multiply
async fn bit_adder<C, S>(
    ctx: C,
    record_id: RecordId,
    x: &AdditiveShare<S>,
    // y: Option<&AdditiveShare<S>>,
    y: &AdditiveShare<S>,
    carry: &mut AdditiveShare<S>,
) -> Result<AdditiveShare<S>, Error>
where
    C: Context,
    S: Field,
{
    // let output = x + y.unwrap_or(&AdditiveShare::<S>::ZERO) + &*carry;
    let output = x + y + &*carry;
    //let output = x+AdditiveShare::<S>::ZERO ;//+ y;// + &*carry;

    *carry = &*carry
        + (x + &*carry)
            // .multiply(&(y.unwrap_or(&AdditiveShare::<S>::ZERO) + &*carry), ctx, record_id)
            .multiply(&(y + &*carry), ctx, record_id)
            .await?;

    Ok(output)
}


#[cfg(all(test, unit_test))]
mod test {
    use rand::Rng;

    use crate::{
        ff::{boolean_array::BA64, Field},
        protocol,
        protocol::{
            context::Context,
            ipa_prf::boolean_ops::addition_low_com::{integer_add, integer_sat_add},
        },
        rand::thread_rng,
        test_executor::run,
        test_fixture::{Reconstruct, Runner, TestWorld},
    };

    ///testing correctness of DY PRF evaluation
    /// by checking MPC generated pseudonym with pseudonym generated in the clear
    #[test]
    fn semi_honest_add() {
        run(|| async move {
            let world = TestWorld::default();

            let mut rng = thread_rng();

            let records: Vec<BA64> = vec![rng.gen::<BA64>(), rng.gen::<BA64>()];
            let x = records[0].as_u128();
            let y = records[1].as_u128();

            let expected = (x + y) % (1+u128::from(u64::MAX));

            let result = world
                .semi_honest(records.into_iter(), |ctx, x_y| async move {
                    integer_add::<_, BA64, BA64>(
                        ctx.set_total_records(1),
                        protocol::RecordId(0),
                        &x_y[0],
                        &x_y[1],
                    )
                    .await
                    .unwrap()
                })
                .await
                .reconstruct()
                .as_u128();
            assert_eq!((x, y, result), (x, y, expected));
        });
    }

    #[test]
    fn semi_honest_sat_add() {
        run(|| async move {
            let world = TestWorld::default();

            let mut rng = thread_rng();

            let records: Vec<BA64> = vec![rng.gen::<BA64>(), rng.gen::<BA64>()];
            let x = records[0].as_u128();
            let y = records[1].as_u128();
            let z = 1+u128::from(u64::MAX);

            let expected = if x + y > z { z-1 } else { (x + y) % z };

            let result = world
                .semi_honest(records.into_iter(), |ctx, x_y| async move {
                    integer_sat_add::<_, BA64, BA64>(
                        ctx.set_total_records(1),
                        protocol::RecordId(0),
                        &x_y[0],
                        &x_y[1],
                    )
                    .await
                    .unwrap()
                })
                .await
                .reconstruct()
                .as_u128();
            assert_eq!((x, y, z, result), (x, y, z, expected));
        });
    }
}
