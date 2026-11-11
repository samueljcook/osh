use crate::bytes::write_length_prefixed;

pub(crate) fn update_fields(update_fn: &mut impl FnMut(&[u8]), domain: &[u8], fields: &[&[u8]]) {
	write_length_prefixed(domain, &mut *update_fn);
	update_fn(&(fields.len() as u64).to_be_bytes());
	for field in fields {
		write_length_prefixed(field, &mut *update_fn);
	}
}
