// Generated by `wit-bindgen-wrpc-go` 0.1.0. DO NOT EDIT!
package process_data

import (
	context "context"
	binary "encoding/binary"
	errors "errors"
	fmt "fmt"
	wrpc "github.com/wrpc/wrpc/go"
	io "io"
	slog "log/slog"
	math "math"
	utf8 "unicode/utf8"
)

type Data struct {
	Name  string
	Count uint32
}

func (v *Data) String() string { return "Data" }

func (v *Data) WriteTo(w wrpc.ByteWriter) error {
	slog.Debug("writing field", "name", "name")
	if err := func(v string, w wrpc.ByteWriter) error {
		n := len(v)
		if n > math.MaxUint32 {
			return fmt.Errorf("string byte length of %d overflows a 32-bit integer", n)
		}
		slog.Debug("writing string byte length", "len", n)
		if err := func(v uint32, w wrpc.ByteWriter) error {
			b := make([]byte, binary.MaxVarintLen32)
			i := binary.PutUvarint(b, uint64(v))
			slog.Debug("writing u32")
			_, err := w.Write(b[:i])
			return err
		}(uint32(n), w); err != nil {
			return fmt.Errorf("failed to write string length of %d: %w", n, err)
		}
		slog.Debug("writing string bytes")
		_, err := w.Write([]byte(v))
		if err != nil {
			return fmt.Errorf("failed to write string bytes: %w", err)
		}
		return nil
	}(v.Name, w); err != nil {
		return fmt.Errorf("failed to write `name` field: %w", err)
	}
	slog.Debug("writing field", "name", "count")
	if err := func(v uint32, w wrpc.ByteWriter) error {
		b := make([]byte, binary.MaxVarintLen32)
		i := binary.PutUvarint(b, uint64(v))
		slog.Debug("writing u32")
		_, err := w.Write(b[:i])
		return err
	}(v.Count, w); err != nil {
		return fmt.Errorf("failed to write `count` field: %w", err)
	}
	return nil
}
func ReadData(r wrpc.ByteReader) (*Data, error) {
	v := &Data{}
	var err error
	slog.Debug("reading field", "name", "name")
	v.Name, err = func(r wrpc.ByteReader) (string, error) {
		var x uint32
		var s uint
		for i := 0; i < 5; i++ {
			slog.Debug("reading string length byte", "i", i)
			b, err := r.ReadByte()
			if err != nil {
				if i > 0 && err == io.EOF {
					err = io.ErrUnexpectedEOF
				}
				return "", fmt.Errorf("failed to read string length byte: %w", err)
			}
			if b < 0x80 {
				if i == 4 && b > 1 {
					return "", errors.New("string length overflows a 32-bit integer")
				}
				x = x | uint32(b)<<s
				buf := make([]byte, x)
				slog.Debug("reading string bytes", "len", x)
				_, err = r.Read(buf)
				if err != nil {
					return "", fmt.Errorf("failed to read string bytes: %w", err)
				}
				if !utf8.Valid(buf) {
					return string(buf), errors.New("string is not valid UTF-8")
				}
				return string(buf), nil
			}
			x |= uint32(b&0x7f) << s
			s += 7
		}
		return "", errors.New("string length overflows a 32-bit integer")
	}(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read `name` field: %w", err)
	}
	slog.Debug("reading field", "name", "count")
	v.Count, err = func(r wrpc.ByteReader) (uint32, error) {
		var x uint32
		var s uint
		for i := 0; i < 5; i++ {
			slog.Debug("reading `uint32` byte", "i", i)
			b, err := r.ReadByte()
			if err != nil {
				if i > 0 && err == io.EOF {
					err = io.ErrUnexpectedEOF
				}
				return x, fmt.Errorf("failed to read `uint32` byte: %w", err)
			}
			if b < 0x80 {
				if i == 4 && b > 1 {
					return x, errors.New("varint overflows a 32-bit integer")
				}
				return x | uint32(b)<<s, nil
			}
			x |= uint32(b&0x7f) << s
			s += 7
		}
		return x, errors.New("varint overflows a 32-bit integer")
	}(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read `count` field: %w", err)
	}
	return v, nil
}

// Send structured data to the component for processing
func Process(ctx__ context.Context, wrpc__ wrpc.Client, data *Data) (r0__ string, err__ error) {
	wrpc__.NewInvocation("wasmcloud:example/process-data", "process")

	//if err != nil {
	//    err__ = fmt.Sprintf("failed to invoke `process`: %w", txErr__)
	//    return
	//}
	//wrpc__.1.await.context("failed to transmit parameters")?;
	//Ok(tx__)
	panic("not supported yet")
	return
}
