use crate::error::{Error, Result};

/// Most tiers a comparison can evaluate at once.
pub const MAX_TIERS: usize = 8;

/// One sensitivity level: how different two pixels must be to count, and how
/// many of them it takes for the images to be reported as different.
///
/// A comparison evaluates every tier in the same scan. A tier with a low
/// colour threshold sees subtle changes but needs many of them; one with a
/// high threshold ignores rendering noise and reports a handful of pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tier {
  /// Colour distance a pixel must exceed, on pixelmatch's scale: 0 is strict,
  /// 1 accepts everything.
  pub threshold: f64,
  /// Share of the canvas that must differ, between 0 and 1. Below it the
  /// tier reports nothing.
  pub min_score: f64,
  /// Caps `min_score` at this many pixels, so a large image does not get to
  /// hide a larger change than a small one.
  pub max_ignored_pixels: Option<f64>,
}

impl Tier {
  /// Lowest score at which this tier reports a difference.
  pub(crate) fn floor(&self, pixels: f64) -> f64 {
    match self.max_ignored_pixels {
      Some(max) => self.min_score.min(max / pixels),
      None => self.min_score,
    }
  }
}

/// How differing pixels that are only rendering noise are told apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Antialiasing {
  /// Count every differing pixel.
  Off,
  /// The detector of pixelmatch and odiff: a pixel is anti-aliasing when it
  /// is brighter than one neighbour and darker than another, and one of the
  /// two sits in a flat area. Masks are identical to odiff's, pixel for
  /// pixel, which keeps existing fingerprints valid.
  #[default]
  Classic,
  /// A pixel is anti-aliasing when the two images can be the same edge, or
  /// the same thin stroke, rendered less than a pixel apart between pixels
  /// that did not change. Sees through noise the classic detector reports —
  /// text over a gradient, hairlines snapped to another pixel — and reports
  /// changes it forgives: a recoloured label, an edge moved by a whole pixel.
  /// Masks, and so fingerprints, differ from the classic ones.
  Subpixel,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
  /// Sensitivity levels, in any order. See [`Options::sensitivity`].
  pub tiers: Vec<Tier>,
  /// Leave anti-aliased pixels out of every tier.
  pub antialiasing: Antialiasing,
  /// Round each tier's score to this many decimals of a percentage before
  /// comparing it to its floor; `None` keeps it exact.
  ///
  /// [`Options::sensitivity`] sets `Some(2)`, because that is what Argos has
  /// always done: its scores came from the `odiff` command line, which prints
  /// two decimals. A change under 0.005% of the image therefore scores zero
  /// and is not reported, whatever [`Tier::max_ignored_pixels`] says — on a
  /// 1920x1080 screenshot that is 103 pixels, not 20. Turning the rounding
  /// off makes the tiers mean what they say, and reports changes that were
  /// silently dropped until now.
  pub percent_decimals: Option<u8>,
  /// Compute the fingerprint of the reported mask.
  pub fingerprint: bool,
  /// Look for content that moved rather than changed.
  pub shifts: bool,
  /// Fewest consecutive rows that must move together to count as a shift.
  pub min_shift_rows: u32,
  /// Refuse canvases larger than this many pixels. Each one costs nine bytes.
  pub max_pixels: Option<u64>,
  /// Threads the scan may use. 0 picks for itself: one below a megapixel,
  /// up to four above. The result does not depend on it.
  pub threads: usize,
}

impl Options {
  /// The two tiers Argos compares screenshots with, scaled by one knob
  /// between 0 and 1 where 0.5 is the default and 0 is strict:
  ///
  /// - a tolerant tier that ignores compression and rendering noise, and
  ///   reports as little as twenty differing pixels;
  /// - a colour-sensitive tier that reports faint changes, such as a tint or
  ///   a shadow, once they cover three percent of the image.
  pub fn sensitivity(threshold: f64) -> Self {
    let relative = threshold * 2.0;
    Self {
      tiers: vec![
        Tier {
          threshold: 0.15 * relative,
          min_score: 0.0002 * relative,
          max_ignored_pixels: Some(20.0 * relative),
        },
        Tier {
          threshold: 0.0225 * relative,
          min_score: 0.03 * relative,
          max_ignored_pixels: None,
        },
      ],
      antialiasing: Antialiasing::Classic,
      percent_decimals: Some(2),
      fingerprint: true,
      shifts: true,
      min_shift_rows: 4,
      max_pixels: None,
      threads: 0,
    }
  }

  pub(crate) fn validate(&self) -> Result<()> {
    let invalid = |message: &str| Err(Error::InvalidOptions(message.to_string()));
    if self.tiers.is_empty() || self.tiers.len() > MAX_TIERS {
      return invalid("expected between 1 and 8 tiers");
    }
    for tier in &self.tiers {
      if !(0.0..=1.0).contains(&tier.threshold) {
        return invalid("tier threshold must be between 0 and 1");
      }
      if !(0.0..=1.0).contains(&tier.min_score) {
        return invalid("tier minScore must be between 0 and 1");
      }
      if tier
        .max_ignored_pixels
        .is_some_and(|max| max.is_nan() || max < 0.0)
      {
        return invalid("tier maxIgnoredPixels must not be negative");
      }
    }
    if self.percent_decimals.is_some_and(|decimals| decimals > 12) {
      return invalid("percentDecimals must be at most 12");
    }
    if self.min_shift_rows == 0 {
      return invalid("minShiftRows must be at least 1");
    }
    Ok(())
  }
}

impl Default for Options {
  fn default() -> Self {
    Self::sensitivity(0.5)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_sensitivity_matches_the_documented_tiers() {
    let options = Options::default();
    assert_eq!(
      options.tiers,
      [
        Tier {
          threshold: 0.15,
          min_score: 0.0002,
          max_ignored_pixels: Some(20.0)
        },
        Tier {
          threshold: 0.0225,
          min_score: 0.03,
          max_ignored_pixels: None
        },
      ]
    );
  }

  #[test]
  fn the_pixel_cap_only_lowers_the_floor() {
    let tier = Options::default().tiers[0];
    // 20 px of a megapixel is less than 0.02%; of a thumbnail it is more.
    assert_eq!(tier.floor(1_000_000.0), 20.0 / 1_000_000.0);
    assert_eq!(tier.floor(10_000.0), 0.0002);
  }

  #[test]
  fn rejects_inconsistent_options() {
    let with = |change: fn(&mut Options)| {
      let mut options = Options::default();
      change(&mut options);
      options.validate()
    };
    assert!(with(|_| {}).is_ok());
    assert!(with(|options| options.tiers.clear()).is_err());
    assert!(with(|options| options.tiers[0].threshold = 1.5).is_err());
    assert!(with(|options| options.tiers[0].threshold = f64::NAN).is_err());
    assert!(with(|options| options.tiers[1].min_score = -0.1).is_err());
    assert!(with(|options| options.tiers[0].max_ignored_pixels = Some(f64::NAN)).is_err());
    assert!(with(|options| options.min_shift_rows = 0).is_err());
  }
}
