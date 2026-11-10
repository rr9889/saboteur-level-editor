use crate::sbla::Heightfield;

pub struct HeightGrid<'a> {
    field: &'a Heightfield,
    min: [f32; 3],
    max: [f32; 3],
}

impl<'a> HeightGrid<'a> {
    pub fn new(field: &'a Heightfield, min: [f32; 3], max: [f32; 3]) -> Result<Self, String> {
        if field.w != 10 || field.h != 10 || field.data.len() != 100 {
            return Err("only verified retail 10x10 HEI1 is supported".into());
        }
        if min.iter().chain(&max).any(|value| !value.is_finite())
            || !field.f0.is_finite()
            || !field.f1.is_finite()
        {
            return Err("non-finite HEI1 coordinates or endpoints".into());
        }
        if (max[0] - min[0] - 60.0).abs() > 0.001 || (max[2] - min[2] - 60.0).abs() > 0.001 {
            return Err("HEI1 query requires the verified 60m square".into());
        }
        Ok(Self { field, min, max })
    }

    pub fn node(&self, column: usize, row: usize) -> Option<[f64; 3]> {
        if column >= 10 || row >= 10 {
            return None;
        }
        let step = (f64::from(self.field.f0) - f64::from(self.field.f1)) / 255.0;
        let height = f64::from(self.min[1])
            + f64::from(self.field.f1)
            + f64::from(self.field.data[row * 10 + column]) * step;
        Some([
            f64::from(self.min[0]) + column as f64 * 60.0 / 9.0,
            height,
            f64::from(self.min[2]) + row as f64 * 60.0 / 9.0,
        ])
    }

    fn contains(&self, world_x: f64, world_z: f64) -> bool {
        world_x.is_finite()
            && world_z.is_finite()
            && world_x >= f64::from(self.min[0])
            && world_x <= f64::from(self.max[0])
            && world_z >= f64::from(self.min[2])
            && world_z <= f64::from(self.max[2])
    }

    pub fn bilinear(&self, world_x: f64, world_z: f64) -> Option<f64> {
        if !self.contains(world_x, world_z) {
            return None;
        }
        let column = (world_x - f64::from(self.min[0])) * 9.0 / 60.0;
        let row = (world_z - f64::from(self.min[2])) * 9.0 / 60.0;
        let column_base = (column.floor() as usize).min(8);
        let row_base = (row.floor() as usize).min(8);
        let column_fraction = column - column_base as f64;
        let row_fraction = row - row_base as f64;
        let lower = self.node(column_base, row_base)?[1] * (1.0 - column_fraction)
            + self.node(column_base + 1, row_base)?[1] * column_fraction;
        let upper = self.node(column_base, row_base + 1)?[1] * (1.0 - column_fraction)
            + self.node(column_base + 1, row_base + 1)?[1] * column_fraction;
        Some(lower * (1.0 - row_fraction) + upper * row_fraction)
    }

    pub fn engine_surface(&self, world_x: f64, world_z: f64) -> Option<f64> {
        if !self.contains(world_x, world_z) {
            return None;
        }
        let from_max_x = (f64::from(self.max[0]) - world_x) / (60.0 / 9.0);
        let from_max_z = (f64::from(self.max[2]) - world_z) / (60.0 / 9.0);
        let column = (10.0 - from_max_x.ceil()).clamp(1.0, 9.0) as usize;
        let row = (10.0 - from_max_z.ceil()).clamp(1.0, 9.0) as usize;
        let fraction_x = from_max_x - from_max_x.floor();
        let fraction_z = from_max_z - from_max_z.floor();
        Some(
            self.node(column, row)?[1] * (1.0 - fraction_x) * (1.0 - fraction_z)
                + self.node(column - 1, row)?[1] * fraction_x * (1.0 - fraction_z)
                + self.node(column - 1, row - 1)?[1] * fraction_x * fraction_z
                + self.node(column, row - 1)?[1] * (1.0 - fraction_x) * fraction_z,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_orientation_and_endpoint_units() {
        let mut field = Heightfield {
            w: 10,
            h: 10,
            f0: 110.0,
            f1: -145.0,
            data: vec![0; 100],
        };
        field.data[99] = 255;
        field.data[12] = 30;
        let grid = HeightGrid::new(&field, [-60.0, 5.0, -120.0], [0.0, 5.0, -60.0]).unwrap();
        assert_eq!(grid.node(0, 0), Some([-60.0, -140.0, -120.0]));
        assert_eq!(grid.node(9, 9), Some([0.0, 115.0, -60.0]));
        assert_eq!(grid.node(2, 1).unwrap()[1], -110.0);
        assert!(grid.node(10, 0).is_none());
        assert!(grid.bilinear(0.01, -90.0).is_none());
        assert!(grid.bilinear(f64::INFINITY, -90.0).is_none());
    }

    #[test]
    fn engine_gridline_rule_is_not_silently_normalized() {
        let field = Heightfield {
            w: 10,
            h: 10,
            f0: 255.0,
            f1: 0.0,
            data: (0..100).collect(),
        };
        let grid = HeightGrid::new(&field, [0.0; 3], [60.0, 0.0, 60.0]).unwrap();
        assert_eq!(grid.bilinear(0.0, 0.0), Some(0.0));
        assert_eq!(grid.engine_surface(0.0, 0.0), Some(11.0));
        assert_eq!(grid.bilinear(60.0, 60.0), Some(99.0));
        assert_eq!(grid.engine_surface(60.0, 60.0), Some(99.0));
        assert!(
            (grid.bilinear(3.0, 3.0).unwrap() - grid.engine_surface(3.0, 3.0).unwrap()).abs()
                < 1e-12
        );
    }

    #[test]
    fn rejects_unverified_dimensions_and_bad_bounds() {
        let mut field = Heightfield {
            w: 10,
            h: 10,
            f0: 1.0,
            f1: 0.0,
            data: vec![0; 100],
        };
        assert!(HeightGrid::new(&field, [0.0; 3], [75.0, 0.0, 60.0]).is_err());
        field.w = 2;
        assert!(HeightGrid::new(&field, [0.0; 3], [60.0, 0.0, 60.0]).is_err());
    }
}
